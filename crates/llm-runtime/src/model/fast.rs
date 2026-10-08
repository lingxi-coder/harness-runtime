//! Native To/Ry environment admission, sampled before one SDK preparation.

/// Current model gate. The host supplies a resolved wire identity; global and
/// per-model environment policy precede baked and raw-name fallbacks.
pub fn model_allowed(model: &str) -> bool {
    let overrides = std::env::var(branding::MODEL_CAPABILITIES_ENV).ok();
    let disabled = crate::structured_output::bool_environment(branding::DISABLE_FAST_MODE_ENV);
    lingxi_core::host::model_capabilities::fast_model_allowed(
        model,
        &lingxi_core::host::model_capabilities::normalize_model_id(model),
        overrides.as_deref(),
        !disabled,
        false,
    )
}
