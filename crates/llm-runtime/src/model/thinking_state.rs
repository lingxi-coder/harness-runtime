//! Session base thinking policy plus the nullable Native max-token overlay.

use super::thinking::ThinkingConfig;

/// A JSON number with Native `Number.isInteger` semantics. Keeping the value as
/// an `f64` preserves negative and wide finite integers until Lsn applies the
/// selected request's output ceiling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeIntegerNumber(f64);

impl NativeIntegerNumber {
    #[must_use]
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value.fract() == 0.0).then_some(Self(value))
    }

    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }

    #[must_use]
    pub fn is_zero(self) -> bool {
        self.0 == 0.0
    }
}

/// Shared session thinking state. Base updates preserve the separately managed
/// Native nullable max-token overlay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThinkingState {
    base: ThinkingConfig,
    max_thinking_tokens_override: Option<NativeIntegerNumber>,
}

impl Default for ThinkingState {
    fn default() -> Self {
        Self {
            base: ThinkingConfig::default(),
            max_thinking_tokens_override: None,
        }
    }
}

impl ThinkingState {
    #[must_use]
    pub fn new(base: ThinkingConfig) -> Self {
        Self {
            base,
            max_thinking_tokens_override: None,
        }
    }

    #[must_use]
    pub fn base(self) -> ThinkingConfig {
        self.base
    }

    pub fn set_base(&mut self, base: ThinkingConfig) {
        self.base = base;
    }

    #[must_use]
    pub fn max_thinking_tokens_override(self) -> Option<NativeIntegerNumber> {
        self.max_thinking_tokens_override
    }

    pub fn set_max_thinking_tokens_override(&mut self, value: Option<NativeIntegerNumber>) {
        self.max_thinking_tokens_override = value;
    }

    #[must_use]
    pub fn explicitly_disabled(self) -> bool {
        matches!(self.base, ThinkingConfig::Disabled)
            || self
                .max_thinking_tokens_override
                .is_some_and(NativeIntegerNumber::is_zero)
    }

    /// Preserve Native's request `type` fact independently from outer feature
    /// gates such as `LINGXI_DISABLE_THINKING` and model support.
    #[must_use]
    pub fn native_request_type_disabled(self) -> Option<bool> {
        if self
            .max_thinking_tokens_override
            .is_some_and(NativeIntegerNumber::is_zero)
        {
            return Some(true);
        }
        match self.base {
            ThinkingConfig::Disabled => Some(true),
            ThinkingConfig::Adaptive | ThinkingConfig::Enabled { .. } => Some(false),
            ThinkingConfig::Automatic => None,
        }
    }

    /// Apply Native's enabled-branch clamp after `max_tokens` has been resolved
    /// against the selected provider profile. The max-token floor is intentional
    /// and is paired with SDK provenance for the exact request ceiling.
    #[must_use]
    pub fn native_enabled_budget(
        self,
        resolved_output_tokens: u32,
        selected_model_upper_limit: Option<u64>,
    ) -> u32 {
        let explicit = self
            .max_thinking_tokens_override
            .map(NativeIntegerNumber::get)
            .or_else(|| match self.base {
                ThinkingConfig::Enabled { budget_tokens } => Some(f64::from(budget_tokens)),
                _ => None,
            });
        let configured = explicit.unwrap_or_else(|| {
            selected_model_upper_limit.map_or_else(
                || f64::from(resolved_output_tokens.saturating_sub(1)),
                |upper| upper.saturating_sub(1) as f64,
            )
        });
        let request_ceiling = f64::from(resolved_output_tokens.saturating_sub(1));
        configured.min(request_ceiling).max(f64::from(
            lingxi_llm_client::providers::anthropic::request_policy::MIN_MANUAL_THINKING_TOKENS,
        )) as u32
    }

    /// Map the overlay through the provider-neutral thinking API. Native Main
    /// requests resolve their exact `Lsn` policy separately; other routes keep
    /// the existing `ThinkingConfig` mapping and must not silently narrow a
    /// JavaScript integer that this API cannot represent.
    pub fn generic_request_config(self) -> Result<ThinkingConfig, NativeIntegerNumber> {
        let Some(override_value) = self.max_thinking_tokens_override else {
            return Ok(self.base);
        };
        if override_value.is_zero() {
            return Ok(ThinkingConfig::Disabled);
        }
        let value = override_value.get();
        if value > 0.0 && value <= f64::from(u32::MAX) {
            return Ok(ThinkingConfig::Enabled {
                budget_tokens: value as u32,
            });
        }
        Err(override_value)
    }

    #[must_use]
    pub fn native_override_is_enabled(self) -> bool {
        self.max_thinking_tokens_override
            .is_some_and(|value| !value.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integer_number_preserves_native_negative_and_wide_integer_domain() {
        assert_eq!(NativeIntegerNumber::new(-7.0).unwrap().get(), -7.0);
        assert_eq!(
            NativeIntegerNumber::new(9_007_199_254_740_992.0)
                .unwrap()
                .get(),
            9_007_199_254_740_992.0
        );
        assert!(NativeIntegerNumber::new(2.5).is_none());
        assert!(NativeIntegerNumber::new(f64::INFINITY).is_none());
    }

    #[test]
    fn overlay_is_independent_from_base_and_lsn_clamps_against_resolved_output() {
        let mut state = ThinkingState::new(ThinkingConfig::Enabled {
            budget_tokens: 2_048,
        });
        state.set_max_thinking_tokens_override(Some(NativeIntegerNumber::new(0.0).unwrap()));
        assert!(state.explicitly_disabled());
        state.set_max_thinking_tokens_override(None);
        assert_eq!(
            state.base(),
            ThinkingConfig::Enabled {
                budget_tokens: 2_048
            }
        );
        assert_eq!(state.native_enabled_budget(512, Some(32_000)), 1_024);

        state.set_max_thinking_tokens_override(Some(NativeIntegerNumber::new(-9.0).unwrap()));
        assert_eq!(state.native_enabled_budget(24_000, Some(128_000)), 1_024);
        state.set_max_thinking_tokens_override(Some(
            NativeIntegerNumber::new(9_007_199_254_740_992.0).unwrap(),
        ));
        assert_eq!(state.native_enabled_budget(8_192, Some(128_000)), 8_191);

        let default = ThinkingState::default();
        assert_eq!(default.native_enabled_budget(64_000, Some(16_000)), 15_999);
        let explicit = ThinkingState {
            max_thinking_tokens_override: Some(NativeIntegerNumber::new(30_000.0).unwrap()),
            ..default
        };
        assert_eq!(explicit.native_enabled_budget(64_000, Some(16_000)), 30_000);
    }

    #[test]
    fn generic_provider_mapping_preserves_existing_config_domain() {
        let base = ThinkingConfig::Enabled {
            budget_tokens: 1_000,
        };
        let mut state = ThinkingState::new(base);
        assert_eq!(state.generic_request_config(), Ok(base));

        state.set_max_thinking_tokens_override(Some(NativeIntegerNumber::new(0.0).unwrap()));
        assert_eq!(state.generic_request_config(), Ok(ThinkingConfig::Disabled));
        state.set_max_thinking_tokens_override(Some(NativeIntegerNumber::new(4_096.0).unwrap()));
        assert_eq!(
            state.generic_request_config(),
            Ok(ThinkingConfig::Enabled {
                budget_tokens: 4_096
            })
        );
        state.set_max_thinking_tokens_override(None);
        assert_eq!(state.generic_request_config(), Ok(base));

        state.set_max_thinking_tokens_override(Some(NativeIntegerNumber::new(-1.0).unwrap()));
        assert!(state.generic_request_config().is_err());
        state.set_max_thinking_tokens_override(Some(
            NativeIntegerNumber::new(f64::from(u32::MAX) + 1.0).unwrap(),
        ));
        assert!(state.generic_request_config().is_err());
    }
}
