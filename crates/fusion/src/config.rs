//! Runtime Fusion settings with defaults applied.

pub use lingxi_core::settings::schema::FusionCompletionPolicy;
use lingxi_core::settings::schema::FusionSettingsJson;
use platform_api::{
    FusionError, FusionModelChoice, FusionModelRole, FusionPreset, FUSION_MAX_PANEL,
    FUSION_MIN_PANEL,
};

/// Resolved Fusion knobs. Invalid *present* settings fail construction;
/// missing fields take the documented defaults.
// This is a flat settings snapshot (mirrors `FusionSettingsJson` field-for-field);
// the bools are independent toggles loaded from user config, not a state
// machine, so collapsing them into enums would only add indirection.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FusionRuntimeConfig {
    /// Agent listing master switch.
    pub enabled: bool,
    /// Default preset for all entrypoints when the caller omits one.
    pub default_preset: FusionPreset,
    /// Quality preset panel count.
    pub quality_panel_count: u8,
    /// Fast preset panel count.
    pub fast_panel_count: u8,
    /// Hard cap 2..=8.
    pub max_panel: u8,
    /// Minimum successful panels before analysis.
    pub min_successful_panels: u8,
    /// Continue when some panels fail.
    pub partial_ok: bool,
    /// Wait for every panel unless quorum completion is explicitly enabled.
    pub completion_policy: FusionCompletionPolicy,
    /// Per-panel turn cap.
    pub panel_max_turns: u32,
    /// Per-turn output token cap.
    pub panel_max_output_tokens_per_turn: u32,
    /// Per-turn reserved input tokens (budget formula; PR3).
    pub panel_reserved_input_tokens_per_turn: u32,
    /// Optional hard reservation ceiling.
    pub max_reserved_nano_usd: Option<u64>,
    /// Analyst output cap.
    pub analyst_max_output_tokens: u32,
    /// Panel idle timeout.
    pub panel_idle_timeout_ms: u64,
    /// Panel total timeout.
    pub panel_total_timeout_ms: u64,
    /// Analyst timeout.
    pub analyst_timeout_ms: u64,
    /// End-to-end timeout.
    pub total_timeout_ms: u64,
    /// Analyst protocol retries (0 or 1).
    pub analysis_protocol_retries: u8,
    /// `/fusion` default cross-provider.
    pub slash_cross_provider_default: bool,
    /// Agent may request cross-provider.
    pub allow_cross_provider_for_agent: bool,
    /// Hard allowlist of profile names. Empty = unrestricted.
    pub allowed_profiles: Vec<String>,
    /// Configured panel roster in the operator's priority order. A preset takes
    /// the first `quality_panel_count` / `fast_panel_count` entries. Empty
    /// means UNCONFIGURED — preflight fails with
    /// [`FusionError::NotConfigured`] rather than choosing models itself.
    pub panel_models: Vec<FusionModelChoice>,
    /// Configured analyst. `None` means unconfigured.
    pub analyst_model: Option<FusionModelChoice>,
}

impl FusionRuntimeConfig {
    /// Documented defaults (enabled stays false).
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            enabled: false,
            default_preset: FusionPreset::Quality,
            quality_panel_count: 3,
            fast_panel_count: 2,
            max_panel: FUSION_MAX_PANEL,
            min_successful_panels: FUSION_MIN_PANEL,
            partial_ok: true,
            completion_policy: FusionCompletionPolicy::WaitAll,
            panel_max_turns: 12,
            panel_max_output_tokens_per_turn: 8192,
            panel_reserved_input_tokens_per_turn: 32768,
            max_reserved_nano_usd: None,
            analyst_max_output_tokens: 8192,
            panel_idle_timeout_ms: 180_000,
            panel_total_timeout_ms: 600_000,
            analyst_timeout_ms: 120_000,
            // F004: must stay >= panelTotal + analystTimeoutMs*(1 +
            // analysisProtocolRetries), or the documented per-stage defaults
            // can never all complete before the end-to-end deadline fires
            // (600_000 + 120_000*2 = 840_000). Raised rather than
            // shrinking the stage defaults, which are independently
            // documented budgets. `FusionSettingsJson::validate` enforces the
            // same inequality per-file when a file itself sets any of the
            // stage fields; `from_settings` below re-enforces it on the
            // fully merged, concrete view, since a merge of files that each
            // individually pass validation is not guaranteed to.
            total_timeout_ms: 1_200_000,
            analysis_protocol_retries: 1,
            slash_cross_provider_default: true,
            allow_cross_provider_for_agent: false,
            allowed_profiles: Vec::new(),
            // Deliberately empty: there is no default model roster. Fusion
            // spends real money on every panel, and a default would mean the
            // set of models a run bills against could change under a working
            // configuration whenever a checked-in table or the live catalog
            // moved. `FusionError::NotConfigured` names the settings keys.
            panel_models: Vec::new(),
            analyst_model: None,
        }
    }

    /// Roles that are still unconfigured, in [`FusionModelRole::ALL`] order.
    #[must_use]
    pub fn missing_model_roles(&self) -> Vec<FusionModelRole> {
        let mut missing = Vec::new();
        if self.panel_models.len() < usize::from(FUSION_MIN_PANEL) {
            missing.push(FusionModelRole::Panels);
        }
        if self.analyst_model.is_none() {
            missing.push(FusionModelRole::Analyst);
        }
        missing
    }

    /// Apply a settings snapshot on top of [`Self::defaults`].
    ///
    /// # Errors
    ///
    /// Returns [`FusionError::InvalidConfiguration`] when present values fail
    /// [`FusionSettingsJson::validate`].
    pub fn from_settings(settings: &FusionSettingsJson) -> Result<Self, FusionError> {
        settings
            .validate()
            .map_err(|err| FusionError::InvalidConfiguration(err.to_string()))?;
        let mut cfg = Self::defaults();
        if let Some(enabled) = settings.enabled {
            cfg.enabled = enabled;
        }
        if let Some(preset) = settings.preset.as_deref() {
            cfg.default_preset = match preset {
                "fast" => FusionPreset::Fast,
                "quality" => FusionPreset::Quality,
                _ => unreachable!("FusionSettingsJson::validate rejected the preset"),
            };
        }
        if let Some(n) = settings.quality_panel_count {
            cfg.quality_panel_count = n;
        }
        if let Some(n) = settings.fast_panel_count {
            cfg.fast_panel_count = n;
        }
        if let Some(n) = settings.max_panel {
            cfg.max_panel = n;
        }
        if let Some(n) = settings.min_successful_panels {
            cfg.min_successful_panels = n;
        }
        if let Some(v) = settings.partial_ok {
            cfg.partial_ok = v;
        }
        if let Some(policy) = settings.completion_policy {
            cfg.completion_policy = policy;
        }
        if let Some(n) = settings.panel_max_turns {
            cfg.panel_max_turns = n;
        }
        if let Some(n) = settings.panel_max_output_tokens_per_turn {
            cfg.panel_max_output_tokens_per_turn = n;
        }
        if let Some(n) = settings.panel_reserved_input_tokens_per_turn {
            cfg.panel_reserved_input_tokens_per_turn = n;
        }
        cfg.max_reserved_nano_usd = settings.max_reserved_nano_usd;
        if let Some(n) = settings.analyst_max_output_tokens {
            cfg.analyst_max_output_tokens = n;
        }
        if let Some(n) = settings.panel_idle_timeout_ms {
            cfg.panel_idle_timeout_ms = n;
        }
        if let Some(n) = settings.panel_total_timeout_ms {
            cfg.panel_total_timeout_ms = n;
        }
        if let Some(n) = settings.analyst_timeout_ms {
            cfg.analyst_timeout_ms = n;
        }
        if let Some(n) = settings.total_timeout_ms {
            cfg.total_timeout_ms = n;
        }
        if let Some(n) = settings.analysis_protocol_retries {
            cfg.analysis_protocol_retries = n;
        }
        if let Some(v) = settings.slash_cross_provider_default {
            cfg.slash_cross_provider_default = v;
        }
        if let Some(v) = settings.allow_cross_provider_for_agent {
            cfg.allow_cross_provider_for_agent = v;
        }
        if let Some(ref names) = settings.allowed_profiles {
            cfg.allowed_profiles.clone_from(names);
        }
        if let Some(ref panels) = settings.panel_models {
            cfg.panel_models = panels.iter().map(choice_from_settings).collect();
        }
        cfg.analyst_model = settings.analyst_model.as_ref().map(choice_from_settings);

        // F004 / F011 item 6 (round-3 review fix): `FusionSettingsJson::validate`
        // above only checked THIS settings snapshot's own fields — and, per its
        // per-file relaxations, deliberately stays silent about the stage-sum
        // and min-successful-vs-preset invariants whenever a single tier sets
        // only one side of the comparison. That is correct for validating one
        // file in isolation (`read_layer_or_skip` must not drop a whole tier
        // just because it doesn't happen to also restate an unrelated field),
        // but `cfg` here is the fully MERGED, concrete view across every tier,
        // and nothing else re-checks these two invariants on it. Enforce both
        // here, after every field above has taken its default or override.
        if !cfg.partial_ok && cfg.completion_policy == FusionCompletionPolicy::QuorumAfterGrace {
            return Err(FusionError::InvalidConfiguration(
                "fusion.completionPolicy quorum_after_grace requires fusion.partialOk=true".into(),
            ));
        }
        let stage_sum = cfg.panel_total_timeout_ms.saturating_add(
            cfg.analyst_timeout_ms
                .saturating_mul(1 + u64::from(cfg.analysis_protocol_retries)),
        );
        if stage_sum > cfg.total_timeout_ms {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion.panelTotalTimeoutMs + fusion.analystTimeoutMs*(1+fusion.analysisProtocolRetries) ({stage_sum}) must not exceed fusion.totalTimeoutMs ({total})",
                total = cfg.total_timeout_ms
            )));
        }
        // No merged-view re-check for the roster bounds, unlike the two
        // invariants below. Those need it because the value on one side of the
        // comparison can come from a DEFAULT that no file states, which the
        // per-file validator cannot see. The roster's two bounds
        // (`minSuccessfulPanels`, `maxPanel`) are both plain fields of this
        // same merged struct, and `validate()` above has already run on it, so
        // a re-check here could never fire — and a branch that cannot fire
        // reads like a guard while protecting nothing.
        let smallest_preset = cfg.quality_panel_count.min(cfg.fast_panel_count);
        if cfg.min_successful_panels > smallest_preset {
            return Err(FusionError::InvalidConfiguration(format!(
                "fusion.minSuccessfulPanels ({min_successful}) must not exceed min(fusion.qualityPanelCount, fusion.fastPanelCount) ({smallest_preset})",
                min_successful = cfg.min_successful_panels
            )));
        }

        // Round-3 review finding [24] (rework, then reworked again after the
        // reviewer rejected the first rework): `FusionSettingsJson::validate`
        // only checks `panelIdleTimeoutMs` against `panelTotalTimeoutMs`
        // same-file (core/src/settings/schema.rs has the full rationale), so
        // a tier that sets only `panelIdleTimeoutMs` can still merge, here,
        // above the effective `panel_total_timeout_ms` — whether that came
        // from another tier or from the DEFAULT. The first rework compared
        // `cfg.panel_idle_timeout_ms` (the merged, defaulted value —
        // 180_000 whenever no tier set it) against `cfg.panel_total_timeout_ms`
        // unconditionally, which rejects any settings file that lowers
        // `panelTotalTimeoutMs` below 180_000 without also lowering
        // `panelIdleTimeoutMs`, even though that operator never touched the
        // idle field at all — the exact false-rejection class this check
        // exists to avoid. Gate it on `settings.panel_idle_timeout_ms`
        // (the pre-default, merged-across-tiers Option: `Some` only when
        // some tier actually set the field) so a merged DEFAULT idle can
        // never trigger this rejection — only a value an operator actually
        // wrote. `panel::panel_stall_timeout_ms` still clamps at the spawn
        // use site as defense in depth (a config built directly in-process,
        // bypassing `from_settings`, still gets a usable watchdog deadline).
        if let Some(idle) = settings.panel_idle_timeout_ms {
            if idle > cfg.panel_total_timeout_ms {
                return Err(FusionError::InvalidConfiguration(format!(
                    "fusion.panelIdleTimeoutMs ({idle}) must not exceed fusion.panelTotalTimeoutMs ({panel_total})",
                    panel_total = cfg.panel_total_timeout_ms
                )));
            }
        }

        Ok(cfg)
    }
}

/// Cross the one seam `core` and `platform-api` cannot share a type across.
/// Trimmed here so a settings file with stray whitespace around a model id
/// cannot produce a route that matches no catalog row.
fn choice_from_settings(
    entry: &lingxi_core::settings::schema::FusionModelSelectionJson,
) -> FusionModelChoice {
    FusionModelChoice::new(entry.profile.trim(), entry.model.trim())
}

impl Default for FusionRuntimeConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Reload point for [`FusionRuntimeConfig`] (F007).
///
/// `FusionOrchestrator` used to receive a `FusionRuntimeConfig` by value at
/// construction and never re-read it: a settings-file edit, a managed-policy
/// change, or `fusion.enabled=false` (the design's §11 kill switch) had no
/// effect on the session's already-built orchestrator until a restart. A
/// `FusionConfigSource` is consulted at the start of every `run()` and every
/// `agent_surface()` call instead, so the next
/// call — not the next restart — sees a settings change.
pub trait FusionConfigSource: Send + Sync {
    /// Reload the current effective config.
    ///
    /// # Errors
    ///
    /// Returns [`FusionError::InvalidConfiguration`] when the underlying
    /// settings snapshot fails to load or fails
    /// [`FusionSettingsJson::validate`].
    fn load(&self) -> Result<FusionRuntimeConfig, FusionError>;
}

/// A fixed config never reloads — the pre-F007 behavior, kept as the
/// zero-ceremony source for tests and any host that has not wired live
/// settings reload.
impl FusionConfigSource for FusionRuntimeConfig {
    fn load(&self) -> Result<FusionRuntimeConfig, FusionError> {
        Ok(self.clone())
    }
}

/// Any `Fn() -> Result<FusionRuntimeConfig, FusionError>` closure is a valid
/// source — the composition root can wrap a revision-cached settings loader
/// without a bespoke struct.
impl<F> FusionConfigSource for F
where
    F: Fn() -> Result<FusionRuntimeConfig, FusionError> + Send + Sync,
{
    fn load(&self) -> Result<FusionRuntimeConfig, FusionError> {
        self()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_policy_defaults_and_merged_partial_rejection() {
        assert_eq!(
            FusionRuntimeConfig::defaults().completion_policy,
            FusionCompletionPolicy::WaitAll
        );
        assert_eq!(
            FusionRuntimeConfig::from_settings(&FusionSettingsJson::default())
                .unwrap()
                .completion_policy,
            FusionCompletionPolicy::WaitAll
        );
        let quorum = FusionSettingsJson {
            completion_policy: Some(FusionCompletionPolicy::QuorumAfterGrace),
            ..Default::default()
        };
        quorum.validate().unwrap();
        assert_eq!(
            FusionRuntimeConfig::from_settings(&quorum)
                .unwrap()
                .completion_policy,
            FusionCompletionPolicy::QuorumAfterGrace
        );
        let no_partial = FusionSettingsJson {
            partial_ok: Some(false),
            ..Default::default()
        };
        no_partial.validate().unwrap();
        let merged = FusionSettingsJson {
            partial_ok: no_partial.partial_ok,
            ..quorum
        };
        assert!(matches!(
            FusionRuntimeConfig::from_settings(&merged),
            Err(FusionError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn defaults_keep_fusion_disabled() {
        let cfg = FusionRuntimeConfig::defaults();
        assert!(!cfg.enabled);
        assert_eq!(cfg.default_preset, FusionPreset::Quality);
        assert_eq!(cfg.quality_panel_count, 3);
        assert_eq!(cfg.fast_panel_count, 2);
        assert_eq!(cfg.max_panel, 8);
        assert_eq!(cfg.panel_max_turns, 12);
        assert_eq!(cfg.panel_reserved_input_tokens_per_turn, 32768);
    }

    #[test]
    fn settings_preset_becomes_the_runtime_default() {
        let settings = FusionSettingsJson {
            preset: Some("fast".into()),
            ..FusionSettingsJson::default()
        };
        let cfg = FusionRuntimeConfig::from_settings(&settings).expect("valid settings");
        assert_eq!(cfg.default_preset, FusionPreset::Fast);
    }

    /// F004 regression (round-3 review): a merged settings snapshot that sets
    /// only `totalTimeoutMs` passes `FusionSettingsJson::validate` (the
    /// stage-sum check there is a per-FILE relaxation, deliberately silent
    /// when none of the stage fields are present in the same file) but
    /// must still be rejected by `from_settings`, which sees the fully
    /// merged, concrete stage values and must enforce the stage-sum
    /// invariant there instead.
    #[test]
    fn from_settings_rejects_a_merged_total_timeout_that_cannot_fit_the_default_stages() {
        let settings = FusionSettingsJson {
            total_timeout_ms: Some(500_000),
            ..FusionSettingsJson::default()
        };
        // The per-file relaxation must still accept this file standalone —
        // proving the file itself is not being dropped.
        settings.validate().expect("single-field file stays valid");
        let err = FusionRuntimeConfig::from_settings(&settings)
            .expect_err("merged stage sum (840_000) exceeds totalTimeoutMs (500_000)");
        match err {
            FusionError::InvalidConfiguration(msg) => {
                assert!(
                    msg.contains("panelTotalTimeoutMs")
                        && msg.contains("analystTimeoutMs")
                        && msg.contains("totalTimeoutMs"),
                    "error must name the offending fields, got: {msg}"
                );
            }
            other => panic!("expected InvalidConfiguration, got {other:?}"),
        }
    }

    /// F011 item 6 regression (round-3 review): a merged settings snapshot
    /// that sets only `minSuccessfulPanels` passes
    /// `FusionSettingsJson::validate` (same per-file relaxation) but must
    /// still be rejected by `from_settings` once merged against the
    /// concrete, default panel counts — otherwise `check_panel_bar` silently
    /// clamps a bar the operator explicitly asked for.
    #[test]
    fn from_settings_rejects_a_merged_min_successful_above_both_default_presets() {
        let settings = FusionSettingsJson {
            min_successful_panels: Some(3),
            ..FusionSettingsJson::default()
        };
        settings.validate().expect("single-field file stays valid");
        let err = FusionRuntimeConfig::from_settings(&settings)
            .expect_err("merged min_successful_panels (3) exceeds min(quality=3, fast=2) = 2");
        match err {
            FusionError::InvalidConfiguration(msg) => {
                assert!(
                    msg.contains("minSuccessfulPanels")
                        && msg.contains("qualityPanelCount")
                        && msg.contains("fastPanelCount"),
                    "error must name the offending fields, got: {msg}"
                );
            }
            other => panic!("expected InvalidConfiguration, got {other:?}"),
        }
    }

    /// Round-3 review finding [24] (rework): the finding's own reproducer —
    /// a tier that sets only `panelIdleTimeoutMs`, with no
    /// `panelTotalTimeoutMs` in the same file — passes
    /// `FusionSettingsJson::validate`'s same-file-only check (that check
    /// deliberately stays silent when the counterpart field isn't in this
    /// file) but must still be rejected by `from_settings` once merged
    /// against the concrete `panel_total_timeout_ms` default (600_000),
    /// otherwise the stall watchdog is handed a deadline it can never reach.
    #[test]
    fn from_settings_rejects_a_merged_panel_idle_timeout_above_the_default_panel_total() {
        let settings = FusionSettingsJson {
            panel_idle_timeout_ms: Some(5_000_000),
            ..FusionSettingsJson::default()
        };
        settings.validate().expect("single-field file stays valid");
        let err = FusionRuntimeConfig::from_settings(&settings).expect_err(
            "merged panel_idle_timeout_ms (5_000_000) exceeds the default panel_total_timeout_ms (600_000)",
        );
        match err {
            FusionError::InvalidConfiguration(msg) => {
                assert!(
                    msg.contains("panelIdleTimeoutMs") && msg.contains("panelTotalTimeoutMs"),
                    "error must name the offending fields, got: {msg}"
                );
            }
            other => panic!("expected InvalidConfiguration, got {other:?}"),
        }
    }

    /// Round-3 review finding [24], rework-rejection regression: a valid
    /// short-deadline config that never mentions `panelIdleTimeoutMs`
    /// anywhere must not be rejected just because the merged view's DEFAULT
    /// `panel_idle_timeout_ms` (180_000) exceeds this file's own, smaller
    /// `panelTotalTimeoutMs`. This is exactly the reviewer's counter-example
    /// config for the first rework, which compared the merged/defaulted
    /// `cfg.panel_idle_timeout_ms` unconditionally instead of gating on
    /// whether the operator actually set the field.
    #[test]
    fn from_settings_accepts_short_deadline_config_that_never_sets_panel_idle_timeout() {
        let settings = FusionSettingsJson {
            total_timeout_ms: Some(100_000),
            panel_total_timeout_ms: Some(60_000),
            analyst_timeout_ms: Some(10_000),
            analysis_protocol_retries: Some(0),
            ..FusionSettingsJson::default()
        };
        settings.validate().expect("single-field file stays valid");
        FusionRuntimeConfig::from_settings(&settings).expect(
            "a config that never sets panelIdleTimeoutMs must not be rejected because the \
             merged DEFAULT panel_idle_timeout_ms (180_000) exceeds this file's own smaller \
             panelTotalTimeoutMs (60_000)",
        );
    }

    // ---- model roles ---------------------------------------------------

    fn roles_json() -> FusionSettingsJson {
        serde_json::from_str(
            r#"{
                "panelModels":[
                    {"profile":"anthropic","model":" claude-opus-5 "},
                    {"profile":"openai","model":"gpt-5.6-sol"}
                ],
                "analystModel":{"profile":"openai","model":"gpt-5.6-terra"}
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn defaults_configure_no_models_at_all() {
        let cfg = FusionRuntimeConfig::defaults();
        assert!(cfg.panel_models.is_empty());
        assert_eq!(cfg.analyst_model, None);
        assert_eq!(cfg.missing_model_roles(), FusionModelRole::ALL.to_vec());
    }

    #[test]
    fn from_settings_reads_every_role_and_trims_stray_whitespace() {
        let cfg = FusionRuntimeConfig::from_settings(&roles_json()).unwrap();
        assert_eq!(
            cfg.panel_models,
            vec![
                FusionModelChoice::new("anthropic", "claude-opus-5"),
                FusionModelChoice::new("openai", "gpt-5.6-sol"),
            ],
            "a padded model id would match no catalog row"
        );
        assert_eq!(
            cfg.analyst_model,
            Some(FusionModelChoice::new("openai", "gpt-5.6-terra"))
        );
        assert!(cfg.missing_model_roles().is_empty());
    }

    /// Whichever tier the two halves came from, the MERGED struct is what
    /// `from_settings` validates — so a roster that can never meet the bar is
    /// refused at load rather than after spending on the panels it did start.
    #[test]
    fn a_roster_that_cannot_meet_the_merged_bar_never_builds_a_runtime_config() {
        let mut settings = roles_json();
        settings.min_successful_panels = Some(3);
        settings.quality_panel_count = Some(3);
        settings.fast_panel_count = Some(3);
        let error = FusionRuntimeConfig::from_settings(&settings)
            .expect_err("a two-model roster cannot satisfy a bar of three");
        assert!(
            error
                .to_string()
                .contains("fewer than fusion.minSuccessfulPanels"),
            "{error}"
        );
    }

    #[test]
    fn a_roster_above_the_merged_panel_cap_never_builds_a_runtime_config() {
        let mut settings = roles_json();
        settings.max_panel = Some(2);
        settings.panel_models.as_mut().unwrap().push(
            lingxi_core::settings::schema::FusionModelSelectionJson {
                profile: "google".into(),
                model: "gemini-3-pro".into(),
            },
        );
        let error = FusionRuntimeConfig::from_settings(&settings)
            .expect_err("a roster above the cap could never be spawned in full");
        assert!(
            error.to_string().contains("exceeding fusion.maxPanel"),
            "{error}"
        );
    }

    /// `core` and `platform-api` cannot depend on each other, so the settings
    /// READER (`FusionSettingsJson`) and the settings WRITER the UIs use
    /// (`platform_api::fusion_setup`) spell the same keys twice. This crate is
    /// the one that sees both: it fails the moment either side is renamed.
    #[test]
    fn the_settings_reader_and_the_setup_writer_agree() {
        use platform_api::fusion_setup::FusionModelRoles;

        let roles = FusionModelRoles {
            panels: vec![
                FusionModelChoice::new("anthropic", "claude-opus-5"),
                FusionModelChoice::new("openai", "gpt-5.6-sol"),
            ],
            analyst: Some(FusionModelChoice::new("openai", "gpt-5.6-terra")),
        };
        let mut written = serde_json::json!({});
        roles.write_settings_json(&mut written);

        // Read what the wizard wrote back through the ENGINE's own typed path,
        // exactly as a settings load would.
        let settings: lingxi_core::settings::schema::SettingsJson =
            serde_json::from_value(written.clone()).expect("the written file parses");
        settings
            .validate()
            .expect("what the wizard writes must pass the engine's validator");
        let cfg = FusionRuntimeConfig::from_settings(
            settings
                .fusion
                .as_ref()
                .expect("the writer produced a `fusion` block"),
        )
        .expect("the written roles build a runtime config");

        assert_eq!(cfg.panel_models, roles.panels);
        assert_eq!(cfg.analyst_model, roles.analyst);
        assert!(
            cfg.missing_model_roles().is_empty(),
            "a file the wizard just completed must not still read as unconfigured"
        );

        // And the reverse direction: what the engine considers configured is
        // what the wizard will show as configured.
        assert!(FusionModelRoles::from_settings_json(&written).is_configured());
    }

    #[test]
    fn the_enabled_switch_and_the_roles_are_independent_settings() {
        use platform_api::fusion_setup;

        let mut written = serde_json::json!({"fusion": {"enabled": true}});
        fusion_setup::FusionModelRoles {
            panels: vec![
                FusionModelChoice::new("anthropic", "claude-opus-5"),
                FusionModelChoice::new("openai", "gpt-5.6-sol"),
            ],
            analyst: Some(FusionModelChoice::new("openai", "gpt-5.6-terra")),
        }
        .write_settings_json(&mut written);
        let settings: lingxi_core::settings::schema::SettingsJson =
            serde_json::from_value(written).unwrap();
        let cfg = FusionRuntimeConfig::from_settings(settings.fusion.as_ref().unwrap()).unwrap();
        assert!(cfg.enabled, "configuring models must not clear the switch");
    }
}
