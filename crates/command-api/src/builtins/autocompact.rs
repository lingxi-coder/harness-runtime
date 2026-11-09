//! `/autocompact` — report (and explain) the auto-compact window size.
//!
//! Ported from the real 2.1.198 binary. Upstream ships **two** `autocompact`
//! command objects: an interactive `type:"local-jsx"` picker
//! (`"Set how full the context gets before auto-summarizing"`) and a headless
//! `type:"local",supportsNonInteractive:!0,thinClientDispatch:"post-text"`
//! variant (`"Configure the auto-compact window size"`). This handler ports the
//! **headless** variant — the one that produces plain text — since LingXi's
//! command registry is the non-interactive / bridge / headless dispatch path.
//!
//! ## Upstream behaviour (probed from the binary)
//!
//! The headless `call` is `N$m = async (e,t) => e.trim() ? EQt(n,t) : M$m(...)`:
//!
//! * **No argument** → `M$m` renders a status block: the current window (`auto`
//!   / `N tokens (from <ENV>)` / `N tokens (from settings)`), an optional
//!   "currently disabled" line, and two fixed explanation lines (plus an
//!   "Overriding auto…" line when the window is pinned).
//! * **An argument** → `EQt` first short-circuits when the env override is
//!   active (`"<ENV> is set and takes precedence. Unset it to change this
//!   setting."`), otherwise parses `auto`/`reset`/`unset`/`default` or a
//!   `100k–1M` token spec (`odo`) and **persists it to `userSettings`**.
//!
//! The registered handler reads the active provider/model resolution, persists
//! per-model windows to the host-owned user settings file, and applies a
//! successful edit immediately. The environment variable is rebranded from
//! `CLAUDE_CODE_AUTO_COMPACT_WINDOW` to `LINGXI_AUTO_COMPACT_WINDOW`.

use crate::model::{BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::auto_compact::{AutoCompactCommandSnapshot, AutoCompactWindow};
use lingxi_core::host::OrchestratorHandle;
use std::sync::Arc;

/// The env var LingXi honours for the auto-compact window override
/// (`compaction::thresholds::effective_context_window_size`). Rebrand of the
/// binary's `CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
const WINDOW_ENV_VAR: &str = "LINGXI_AUTO_COMPACT_WINDOW";

/// Byte-exact from the binary's `EQt` env-precedence short-circuit (env noun
/// rebranded). Returned when a set-argument is supplied but the env override
/// pins the window.
const ENV_PRECEDENCE_NOTE: &str =
    "LINGXI_AUTO_COMPACT_WINDOW is set and takes precedence. Unset it to change this setting.";

/// Byte-exact `M$m` explanation line #1.
const EXPLAIN_THRESHOLD: &str = "Auto-compact summarizes the conversation when context usage approaches this limit. The actual threshold is the minimum of this setting and your model's maximum context window.";

/// Byte-exact `M$m` explanation line #2.
const EXPLAIN_AUTO_RECOMMENDED: &str = "The auto setting picks a window tuned for your model and is strongly recommended for the best cost and performance.";

/// Byte-exact `M$m` trailing line, emitted only when the window is pinned
/// (source is env / settings) rather than `auto`.
const EXPLAIN_OVERRIDE_WARNING: &str =
    "Overriding auto may result in high token usage, especially when resuming long sessions.";

/// Where the reported auto-compact window came from — 1:1 with the `source`
/// field of claude-code's `N8` (`resolveAutoCompactWindow`) as consumed by the
/// status renderer `sgT` (cc-238.js @294999853):
///
/// ```text
/// `Auto-compact window: ${o==="auto"?"auto"
///   :o==="experiment"||o==="clientdata"?`${oc(n)} tokens)${i}`
///   :o==="env"?`${oc(n)} tokens (from CLAUDE_CODE_AUTO_COMPACT_WINDOW)${i}`
///   :o==="unknown-model"?`${oc(n)} tokens (default for an unrecognized model)${i}`
///   :o==="model-default"?`${oc(n)} tokens (default for this model)${i}`
///   :`${oc(n)} tokens (from settings)${i}`}`
/// ```
///
/// SC-03: the `unknown-model` and `model-default` arms are **new in 2.1.238**
/// (`" tokens (default for this model)"` has 0 hits in the 2.1.220 binary; the
/// 2.1.220 twin `$ly` @236076164 is the same expression without them).
///
/// Legacy disconnected renderer. Registered commands use the live host snapshot
/// below, which also carries the active model, cap and enabled state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// `source:"auto"` — no override and no model-specific default applied.
    Auto,
    /// `source:"env"` — pinned by `LINGXI_AUTO_COMPACT_WINDOW`.
    Env(u64),
    /// `source:"settings"` — pinned by `settings.autoCompactWindow`.
    Settings(u64),
    /// `source:"model-default"` — the model's own default window (new in 2.1.238).
    ModelDefault(u64),
    /// `source:"unknown-model"` — the assumed window for a model this build does
    /// not recognize (new in 2.1.238).
    UnknownModel(u64),
}

impl WindowSource {
    /// The `Auto-compact window: …` line for this source, byte-exact with `sgT`.
    ///
    /// Unbound handlers have no model cap; registered handlers use `current_status`.
    fn window_line(self) -> String {
        match self {
            Self::Auto => "Auto-compact window: auto".to_string(),
            Self::Env(n) => format!(
                "Auto-compact window: {} tokens (from {WINDOW_ENV_VAR})",
                format_tokens_compact(n)
            ),
            Self::Settings(n) => format!(
                "Auto-compact window: {} tokens (from settings)",
                format_tokens_compact(n)
            ),
            Self::ModelDefault(n) => format!(
                "Auto-compact window: {} tokens (default for this model)",
                format_tokens_compact(n)
            ),
            Self::UnknownModel(n) => format!(
                "Auto-compact window: {} tokens (default for an unrecognized model)",
                format_tokens_compact(n)
            ),
        }
    }

    /// `if(o==="env"||o==="settings") a.push(…)` — the "Overriding auto…" line
    /// is pinned to the two *user*-set sources only; a model default (or an
    /// unrecognized-model fallback) is not an override.
    fn is_user_override(self) -> bool {
        matches!(self, Self::Env(_) | Self::Settings(_))
    }
}

/// `/autocompact` handler for the headless and bridge command surface.
#[derive(Default, Clone)]
pub struct AutocompactHandler {
    handle: Option<Arc<dyn OrchestratorHandle>>,
}

impl AutocompactHandler {
    /// Construct a new `AutocompactHandler`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the selected live model and the host-owned settings destination.
    #[must_use]
    pub fn with_handle(mut self, handle: Arc<dyn OrchestratorHandle>) -> Self {
        self.handle = Some(handle);
        self
    }

    fn current_status(snapshot: &AutoCompactCommandSnapshot) -> String {
        let configured = format_tokens_compact(snapshot.configured);
        let suffix = if snapshot.configured > snapshot.window {
            format!(
                " · capped to {} by model",
                format_tokens_compact(snapshot.window)
            )
        } else {
            String::new()
        };
        let value = match snapshot.source.as_str() {
            "auto" => "auto".to_owned(),
            "experiment" | "clientdata" => format!("auto ({configured} tokens){suffix}"),
            source => {
                let label = match source {
                    "env" => format!("from {WINDOW_ENV_VAR}"),
                    "model-default" => "default for this model".to_owned(),
                    "unknown-model" => "default for an unrecognized model".to_owned(),
                    _ => "from settings".to_owned(),
                };
                format!("{configured} tokens ({label}){suffix}")
            }
        };
        let mut lines = vec![format!(
            "Auto-compact window for {}: {value}",
            snapshot.display_name
        )];
        if let Ok(raw) = std::env::var(WINDOW_ENV_VAR) {
            if let Some(diagnostic) = lingxi_core::host::auto_compact::environment_diagnostic(&raw)
            {
                lines.push(diagnostic);
            }
        }
        if !snapshot.enabled {
            lines.push("Auto-compact is currently disabled (see /config)".into());
        }
        lines.push(EXPLAIN_THRESHOLD.into());
        if snapshot.recommend_auto {
            lines.push(EXPLAIN_AUTO_RECOMMENDED.into());
        }
        if snapshot.override_warning {
            lines.push(EXPLAIN_OVERRIDE_WARNING.into());
        }
        lines.join("\n")
    }

    async fn handle_current(&self, handle: &dyn OrchestratorHandle, argument: &str) -> String {
        let snapshot = match handle.auto_compact_command_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return format!("Couldn't read setting: {error}"),
        };
        if argument.is_empty() {
            return Self::current_status(&snapshot);
        }
        if snapshot.source == "env" {
            return ENV_PRECEDENCE_NOTE.into();
        }
        let Some(window) = parse_window(argument) else {
            return format!("Couldn't parse '{argument}'. Expected 'auto' or 100k–1M tokens (e.g. 500k, 200000, or 200 as shorthand)");
        };
        let Some(path) = snapshot.user_settings_path.as_ref() else {
            return "Couldn't save setting: user settings path is unavailable".into();
        };
        if let Err(error) = persist_window(path, &snapshot.settings_key, window) {
            return format!("Couldn't save setting: {error}");
        }
        if let Err(error) = handle
            .auto_compact_window_changed(&snapshot.settings_key, window)
            .await
        {
            return format!("Couldn't apply setting: {error}");
        }
        let resulting = match handle.auto_compact_command_snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => return format!("Couldn't read setting: {error}"),
        };
        let overridden =
            (resulting.source == "settings").then_some(resulting.configured) != window.tokens();
        let label = format!("Auto-compact window for {}", snapshot.display_name);
        if window == AutoCompactWindow::Auto {
            return if overridden {
                format!("{label} set to auto in settings, but a higher-priority override is active ({} tokens)", format_tokens_compact(resulting.window))
            } else {
                format!("{label} set to auto")
            };
        }
        let tokens = window.tokens().expect("numeric window");
        let suffix = if overridden {
            format!(
                ", but a higher-priority override is active ({} tokens)",
                format_tokens_compact(resulting.window)
            )
        } else if resulting.window < tokens {
            format!(
                " (capped to model limit of {})",
                format_tokens_compact(resulting.window)
            )
        } else {
            String::new()
        };
        format!(
            "{label} set to {} tokens{suffix}",
            format_tokens_compact(tokens)
        )
    }

    /// The current environment override, sharing Native's suffix/prefix parser
    /// and 100k–1M clamp with the live threshold resolver.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn env_window() -> Option<u64> {
        std::env::var(WINDOW_ENV_VAR)
            .ok()
            .and_then(|raw| lingxi_core::host::auto_compact::resolve_env_window(&raw))
    }

    /// Render the no-argument status block (`M$m`) for a resolved window
    /// [`WindowSource`].
    fn status_block(source: WindowSource) -> String {
        let mut lines: Vec<String> = Vec::with_capacity(4);
        lines.push(source.window_line());
        lines.push(EXPLAIN_THRESHOLD.to_string());
        lines.push(EXPLAIN_AUTO_RECOMMENDED.to_string());
        if source.is_user_override() {
            lines.push(EXPLAIN_OVERRIDE_WARNING.to_string());
        }
        lines.join("\n")
    }

    /// Fallback source for an intentionally unbound standalone handler.
    fn resolved_source(window: Option<u64>) -> WindowSource {
        window.map_or(WindowSource::Auto, WindowSource::Env)
    }
}

/// Native 2.1.319 `fa`/`Ur`: compact notation with one fractional digit,
/// lower-cased, then `.0` removed. `200000` → `200k`, `1500000` → `1.5m`.
fn format_tokens_compact(n: u64) -> String {
    // Intl uses half-expand rounding and promotes the suffix when rounding
    // 999.95k to 1.0m. Integer arithmetic preserves those ties exactly.
    let (divisor, suffix) = if n >= 999_950 {
        (1_000_000, "m")
    } else if n >= 1_000 {
        (1_000, "k")
    } else {
        return n.to_string();
    };
    let tenths = n.saturating_add(divisor / 20) / (divisor / 10);
    if tenths % 10 == 0 {
        format!("{}{suffix}", tenths / 10)
    } else {
        format!("{}.{digits}{suffix}", tenths / 10, digits = tenths % 10)
    }
}

#[async_trait]
impl BuiltinCommandHandler for AutocompactHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        if let Some(handle) = self.handle.as_ref() {
            let argument = crate::model::trim_source_text(&args.raw_args);
            let display = self
                .handle_current(handle.as_ref(), argument.string_at("").unwrap_or_default())
                .await;
            return CommandResult::Done {
                display: Some(display.into()),
            };
        }
        let window = Self::env_window();
        let arg = crate::model::trim_source_text(&args.raw_args);
        let has_argument = !arg.string_units("").unwrap_or_default().is_empty();

        let display = if !has_argument {
            // No argument → status block (`M$m`).
            Self::status_block(Self::resolved_source(window))
        } else if window.is_some() {
            // Set-argument while the env override is active → byte-exact
            // env-precedence note (`EQt` short-circuit).
            ENV_PRECEDENCE_NOTE.to_string()
        } else {
            "Couldn't save setting: no active model is bound to this command".to_owned()
        };

        CommandResult::Done {
            display: Some((display).into()),
        }
    }

    fn name(&self) -> &str {
        "autocompact"
    }

    fn description(&self) -> &str {
        "Configure the auto-compact window size"
    }
}

fn parse_window(argument: &str) -> Option<AutoCompactWindow> {
    let argument = lingxi_core::host::effort::trim_js_whitespace(argument).to_lowercase();
    if matches!(argument.as_str(), "auto" | "reset" | "unset" | "default") {
        return Some(AutoCompactWindow::Auto);
    }
    match lingxi_core::host::auto_compact::parse_size(&argument, true) {
        lingxi_core::host::auto_compact::ParsedAutoCompactSize::InRange(tokens) => {
            Some(AutoCompactWindow::Tokens(tokens))
        }
        _ => None,
    }
}

fn persist_window(
    path: &std::path::Path,
    settings_key: &str,
    window: AutoCompactWindow,
) -> Result<(), String> {
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    migrations::settings_update::transform_settings_source_with_receipt(path, |source| {
        let value = Utf16JsonProjection::from_serde_value(
            &serde_json::to_value(window).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if lingxi_core::host::effort_table::object_prototype_key(settings_key) {
            if window == AutoCompactWindow::Auto {
                source
                    .remove_field("autoCompactWindow")
                    .map_err(|error| error.to_string())?;
            } else {
                source
                    .set_field("autoCompactWindow", value)
                    .map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        let empty = || Utf16JsonProjection::parse("{}").expect("empty object");
        let mut models = source
            .subprojection("/modelSettings")
            .ok()
            .filter(|value| value.object_entries().is_some())
            .unwrap_or_else(empty);
        let mut model = models
            .subprojection_property_units(&settings_key.encode_utf16().collect::<Vec<_>>())
            .ok()
            .filter(|value| value.object_entries().is_some())
            .unwrap_or_else(empty);
        model
            .set_field("autoCompactWindow", value)
            .map_err(|error| error.to_string())?;
        models
            .set_field(settings_key, model)
            .map_err(|error| error.to_string())?;
        source
            .set_field("modelSettings", models)
            .map_err(|error| error.to_string())
    })
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;

    /// Serializes the env-mutating tests in this module so parallel test threads
    /// can't race the shared `LINGXI_AUTO_COMPACT_WINDOW` process env.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "autocompact".into(),
            raw_args: raw.into(),
            positional_args: raw.split_whitespace().map(Into::into).collect(),
        }
    }

    #[test]
    fn name_and_description() {
        let h = AutocompactHandler::new();
        assert_eq!(h.name(), "autocompact");
        assert_eq!(h.description(), "Configure the auto-compact window size");
    }

    #[test]
    fn compact_formatter_matches_gl_for_round_values() {
        assert_eq!(format_tokens_compact(200_000), "200k");
        assert_eq!(format_tokens_compact(500_000), "500k");
        assert_eq!(format_tokens_compact(1_000_000), "1m");
        assert_eq!(format_tokens_compact(1_500_000), "1.5m");
        assert_eq!(format_tokens_compact(50_000), "50k");
        assert_eq!(format_tokens_compact(150_001), "150k");
        assert_eq!(format_tokens_compact(150_050), "150.1k");
        assert_eq!(format_tokens_compact(999_950), "1m");
    }

    /// SC-03: the two window-source labels 2.1.238 added to `sgT` are rendered
    /// byte-exactly, and neither one is treated as a user override (so the
    /// "Overriding auto…" line stays off, matching `o==="env"||o==="settings"`).
    #[test]
    fn sc03_new_window_source_labels_are_byte_exact() {
        assert_eq!(
            WindowSource::ModelDefault(200_000).window_line(),
            "Auto-compact window: 200k tokens (default for this model)"
        );
        assert_eq!(
            WindowSource::UnknownModel(200_000).window_line(),
            "Auto-compact window: 200k tokens (default for an unrecognized model)"
        );
        assert_eq!(
            WindowSource::Settings(500_000).window_line(),
            "Auto-compact window: 500k tokens (from settings)"
        );
        assert_eq!(
            WindowSource::Env(1_000_000).window_line(),
            "Auto-compact window: 1m tokens (from LINGXI_AUTO_COMPACT_WINDOW)"
        );
        assert_eq!(
            WindowSource::Auto.window_line(),
            "Auto-compact window: auto"
        );

        for source in [
            WindowSource::Auto,
            WindowSource::ModelDefault(200_000),
            WindowSource::UnknownModel(200_000),
        ] {
            assert!(
                !AutocompactHandler::status_block(source).contains(EXPLAIN_OVERRIDE_WARNING),
                "{source:?} is not a user override"
            );
        }
        for source in [WindowSource::Env(200_000), WindowSource::Settings(200_000)] {
            assert!(
                AutocompactHandler::status_block(source).ends_with(EXPLAIN_OVERRIDE_WARNING),
                "{source:?} is a user override"
            );
        }
    }

    #[tokio::test]
    async fn no_env_no_arg_reports_auto_window() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(WINDOW_ENV_VAR);
        let h = AutocompactHandler::new();
        match h.handle(&args("")).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(s.starts_with("Auto-compact window: auto\n"));
                assert!(s.contains("The actual threshold is the minimum of this setting"));
                assert!(s.contains("strongly recommended for the best cost and performance."));
                // The "Overriding auto…" line only appears when pinned.
                assert!(!s.contains("Overriding auto may result in high token usage"));
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn env_set_no_arg_reports_env_sourced_window() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(WINDOW_ENV_VAR, "200000");
        let h = AutocompactHandler::new();
        match h.handle(&args("")).await {
            CommandResult::Done { display: Some(s) } => {
                assert!(s.starts_with(
                    "Auto-compact window: 200k tokens (from LINGXI_AUTO_COMPACT_WINDOW)\n"
                ));
                assert!(s.ends_with(
                    "Overriding auto may result in high token usage, especially when resuming long sessions."
                ));
            }
            other => panic!("expected Done, got {other:?}"),
        }
        std::env::remove_var(WINDOW_ENV_VAR);
    }

    #[tokio::test]
    async fn env_set_with_arg_returns_precedence_note() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var(WINDOW_ENV_VAR, "500000");
        let h = AutocompactHandler::new();
        match h.handle(&args("200k")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "LINGXI_AUTO_COMPACT_WINDOW is set and takes precedence. Unset it to change this setting.".into()
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
        std::env::remove_var(WINDOW_ENV_VAR);
    }

    #[tokio::test]
    async fn unbound_handler_does_not_claim_to_save_a_window() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var(WINDOW_ENV_VAR);
        let h = AutocompactHandler::new();
        match h.handle(&args("200k")).await {
            CommandResult::Done { display: Some(s) } => {
                assert_eq!(
                    s,
                    "Couldn't save setting: no active model is bound to this command".into()
                );
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn zero_or_garbage_env_falls_back_to_auto() {
        let _g = ENV_LOCK.lock().unwrap();
        for bad in ["0", "abc", ""] {
            std::env::set_var(WINDOW_ENV_VAR, bad);
            let h = AutocompactHandler::new();
            match h.handle(&args("")).await {
                CommandResult::Done { display: Some(s) } => {
                    assert!(
                        s.starts_with("Auto-compact window: auto\n"),
                        "bad={bad}: {s:?}"
                    );
                }
                other => panic!("expected Done, got {other:?}"),
            }
        }
        std::env::remove_var(WINDOW_ENV_VAR);
    }

    #[test]
    fn env_window_matches_latest_js_prefix_and_clamp() {
        let _guard = ENV_LOCK.lock().unwrap();
        for (raw, expected) in [
            ("1", 100_000),
            ("250000 tokens", 250_000),
            ("2000000", 1_000_000),
        ] {
            std::env::set_var(WINDOW_ENV_VAR, raw);
            assert_eq!(AutocompactHandler::env_window(), Some(expected));
        }
        std::env::remove_var(WINDOW_ENV_VAR);
    }

    #[test]
    fn window_parser_matches_native_319_cases_and_command_reset_aliases() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../compaction/tests/fixtures/claude_2_1_319_settings_parser.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let input = case["input"].as_str().unwrap();
            let expected = if matches!(input, "reset" | "unset" | "default") {
                Some(AutoCompactWindow::Auto)
            } else if case["expected"].is_null() {
                None
            } else {
                AutoCompactWindow::from_value(&case["expected"])
            };
            assert_eq!(parse_window(input), expected, "{input:?}");
        }
    }

    #[test]
    fn persisted_window_preserves_sibling_models_settings_and_utf16_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        std::fs::write(&path,
            r#"{"opaque":"\ud800","autoCompactWindow":200000,"modelSettings":{"provider-a/model":{"effortLevel":"high","autoCompactWindow":500000},"provider-b/model":{"autoCompactWindow":300000}}}"#
        ).unwrap();
        persist_window(&path, "provider-a/model", AutoCompactWindow::Auto).unwrap();
        let source = migrations::settings_update::read_settings_source(&path).unwrap();
        assert_eq!(source.string_units("/opaque"), Some(vec![0xd800]));
        assert_eq!(source.number_at("/autoCompactWindow"), Some(200000.0));
        let models = source.subprojection("/modelSettings").unwrap();
        let a = models
            .subprojection_property_units(&"provider-a/model".encode_utf16().collect::<Vec<_>>())
            .unwrap();
        let b = models
            .subprojection_property_units(&"provider-b/model".encode_utf16().collect::<Vec<_>>())
            .unwrap();
        assert_eq!(a.string_at("/autoCompactWindow"), Some("auto"));
        assert_eq!(a.string_at("/effortLevel"), Some("high"));
        assert_eq!(b.number_at("/autoCompactWindow"), Some(300000.0));
        std::fs::write(&path, "{broken").unwrap();
        assert!(
            persist_window(&path, "provider-a/model", AutoCompactWindow::Tokens(500000)).is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn current_status_includes_model_cap_and_disabled_setting() {
        // current_status reads Native's environment diagnostic too. Serialize
        // this reader with the tests that temporarily install malformed knobs.
        let _guard = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os(WINDOW_ENV_VAR);
        std::env::remove_var(WINDOW_ENV_VAR);
        let snapshot = AutoCompactCommandSnapshot {
            settings_key: "provider/model".into(),
            display_name: "Model".into(),
            enabled: false,
            window: 200000,
            configured: 500000,
            source: "settings".into(),
            recommend_auto: true,
            override_warning: false,
            user_settings_path: None,
        };
        let text = AutocompactHandler::current_status(&snapshot);
        assert!(text.starts_with("Auto-compact window for Model: 500k tokens (from settings) · capped to 200k by model\nAuto-compact is currently disabled (see /config)\n"), "unexpected status: {text:?}");
        assert!(!text.contains(EXPLAIN_OVERRIDE_WARNING));
        if let Some(previous) = previous {
            std::env::set_var(WINDOW_ENV_VAR, previous);
        }
    }

    #[tokio::test]
    async fn registered_handler_persists_current_profile_and_reset_applies_immediately() {
        use orchestrator::test_support::{
            noop_hook_executor, MockApiClient, MockOutputStream, NoOpPermissionGate,
            StaticMemoryProvider,
        };
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(WINDOW_ENV_VAR);
        let directory = tempfile::tempdir().unwrap();
        let handle = Arc::new(
            orchestrator::ConversationOrchestrator::new(
                orchestrator::OrchestratorConfig {
                    model: "autocompact-command-test".into(),
                    ..Default::default()
                },
                Arc::new(MockApiClient::new(Vec::new())),
                Arc::new(tool_api::registry::ToolRegistry::new()),
                noop_hook_executor(),
                Arc::new(NoOpPermissionGate),
                Arc::new(MockOutputStream::new()),
                Arc::new(StaticMemoryProvider::empty()),
                directory.path().to_owned(),
            )
            .with_config_home(directory.path().to_owned()),
        );
        handle.session().lock().await.model_profile = Some("provider-a".into());
        let handler = AutocompactHandler::new().with_handle(handle.clone());
        let result = handler.handle(&args("500k")).await;
        let CommandResult::Done {
            display: Some(text),
        } = result
        else {
            panic!("expected command success")
        };
        assert!(text.contains("set to 500k tokens"), "{text:?}");
        let settings = migrations::settings_update::read_settings_source(
            &directory.path().join("settings.json"),
        )
        .unwrap();
        let models = settings.subprojection("/modelSettings").unwrap();
        let row = models
            .subprojection_property_units(
                &"provider-a/autocompact-command-test"
                    .encode_utf16()
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(row.number_at("/autoCompactWindow"), Some(500000.0));
        assert_eq!(
            handle.auto_compact_command_snapshot().await.unwrap().source,
            "settings"
        );
        let result = handler.handle(&args("reset")).await;
        let CommandResult::Done {
            display: Some(text),
        } = result
        else {
            panic!("expected command success")
        };
        assert!(text.ends_with("set to auto"), "{text:?}");
        assert_ne!(
            handle.auto_compact_command_snapshot().await.unwrap().source,
            "settings"
        );
        handle.session().lock().await.model_profile = Some("provider-b".into());
        assert_ne!(
            handle.auto_compact_command_snapshot().await.unwrap().source,
            "settings"
        );
    }
}
