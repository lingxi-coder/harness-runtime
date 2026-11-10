//! Request-scoped tool prompt selection and complete template overrides.

use crate::tool_trait::PromptOptions;
use lingxi_core::host::prompt_policy::{resolve_prompt_policy, PromptTarget, ResolvedPromptPolicy};
use lingxi_core::host::session_context::ConversationMode;
use lingxi_core::types::utf16_json::Utf16JsonProjection;

fn with_policy<T>(options: &PromptOptions, apply: impl FnOnce(&ResolvedPromptPolicy) -> T) -> T {
    if let Some(policy) = options.prompt_policy.as_deref() {
        return apply(policy);
    }
    // Only legacy/standalone consumers use this fallback. The request builders
    // supply the policy resolved from the actual mode, provider and model route.
    let policy = resolve_prompt_policy(PromptTarget::for_model(
        ConversationMode::Code,
        options.model.as_deref().unwrap_or_default(),
    ));
    apply(&policy)
}

/// Whether the request uses the synchronized Claude tool template family.
#[must_use]
pub fn prompt_uses_native(options: &PromptOptions) -> bool {
    with_policy(options, |policy| policy.tools.is_native())
}

/// Select the short tool descriptions from the resolved bundle. Claude keeps
/// its native capability and environment overrides; generic bundles use Lean.
#[must_use]
pub fn prompt_uses_lean(options: &PromptOptions) -> bool {
    with_policy(options, |policy| {
        !policy.tools.is_native()
            || lingxi_core::host::model_capabilities::claude_lean_prompt_for(
                &policy.target.canonical_model,
            )
    })
}

/// Claude-only rewrite capabilities must never leak into a generic bundle,
/// including when a global capability override is present.
#[must_use]
pub fn prompt_uses_rewrite(options: &PromptOptions) -> bool {
    with_policy(options, |policy| {
        policy.tools.is_native()
            && lingxi_core::host::model_capabilities::lean_rewrite_for(
                &policy.target.canonical_model,
            )
    })
}

/// Apply a bundle's complete replacement for one named tool. The unmodified
/// projection is returned on a miss so source UTF-16 units are preserved.
#[must_use]
pub fn apply_prompt_tool_override(
    tool_name: &str,
    options: &PromptOptions,
    description: Utf16JsonProjection,
) -> Utf16JsonProjection {
    with_policy(options, |policy| {
        policy
            .tools
            .template_for(tool_name)
            .map_or(description, Utf16JsonProjection::from)
    })
}

/// Legacy native-Claude selector retained for independent upstream parity
/// checks. Request code uses [`prompt_uses_lean`] with its resolved bundle.
#[must_use]
pub fn dh_simple_system_prompt(model: Option<&str>) -> bool {
    let Some(model) = model.filter(|model| !model.is_empty()) else {
        return false;
    };
    if lingxi_core::host::model_capabilities::prompt_profile_for(model)
        == lingxi_core::host::model_capabilities::PromptProfile::FullHarness
    {
        return false;
    }
    lingxi_core::host::model_capabilities::claude_lean_prompt_for(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lingxi_core::host::prompt_policy::{PromptTemplateOverride, ToolPromptBinding};
    use std::sync::Arc;

    fn options_for(model: &str) -> PromptOptions {
        PromptOptions {
            model: Some(model.into()),
            ..PromptOptions::default()
        }
    }

    #[test]
    fn legacy_callers_resolve_portable_lean_for_other_and_unknown_models() {
        for model in [
            "gpt-5.5",
            "gemini-3.5-flash",
            "kimi-k2.5",
            "deepseek-v4",
            "vendor/unknown-model",
            "vendor/claude-opus-5-5",
            "",
        ] {
            let options = options_for(model);
            assert!(prompt_uses_lean(&options), "{model}");
            assert!(!prompt_uses_native(&options), "{model}");
            assert!(!prompt_uses_rewrite(&options), "{model}");
        }
        assert!(prompt_uses_lean(&PromptOptions::default()));
    }

    #[test]
    fn request_policy_wins_over_a_stale_legacy_model_field() {
        let native = Arc::new(resolve_prompt_policy(PromptTarget::for_model(
            ConversationMode::Code,
            "claude-sonnet-4-6",
        )));
        let generic = Arc::new(resolve_prompt_policy(PromptTarget::for_model(
            ConversationMode::Chat,
            "kimi-k2.5",
        )));
        let native_options = PromptOptions {
            prompt_policy: Some(native),
            ..options_for("gpt-5.5")
        };
        assert!(prompt_uses_native(&native_options));
        assert_eq!(
            prompt_uses_lean(&native_options),
            lingxi_core::host::model_capabilities::claude_lean_prompt_for("claude-sonnet-4-6")
        );
        let generic_options = PromptOptions {
            prompt_policy: Some(generic),
            ..options_for("claude-opus-5-5")
        };
        assert!(!prompt_uses_native(&generic_options));
        assert!(prompt_uses_lean(&generic_options));
        assert!(!prompt_uses_rewrite(&generic_options));
    }

    #[test]
    fn resolved_claude_transports_keep_the_native_tool_style() {
        for model in [
            "claude-opus-5-5",
            "anthropic/claude-opus-5.5",
            "openrouter/anthropic/claude-opus-5-5",
            "us.anthropic.claude-opus-5-5-v1:0",
            "claude-opus-5-5@20261101",
        ] {
            let options = options_for(model);
            assert!(prompt_uses_native(&options), "{model}");
            assert_eq!(
                prompt_uses_lean(&options),
                lingxi_core::host::model_capabilities::claude_lean_prompt_for("claude-opus-5-5"),
                "{model}",
            );
        }
    }

    #[test]
    fn named_tool_overrides_replace_the_complete_description_without_touching_other_sources() {
        let mut policy =
            resolve_prompt_policy(PromptTarget::for_model(ConversationMode::Code, "gpt-5.5"));
        policy.tools = ToolPromptBinding::Lean {
            overrides: &[PromptTemplateOverride {
                key: "Read",
                template: "Model-specific complete Read instructions.",
            }],
        };
        let options = PromptOptions {
            prompt_policy: Some(Arc::new(policy)),
            ..PromptOptions::default()
        };
        let source_units = vec![b'x' as u16, 0xd800, b'y' as u16];
        let source = Utf16JsonProjection::root_string(
            String::from_utf16_lossy(&source_units),
            source_units.clone(),
        )
        .unwrap();
        assert_eq!(
            apply_prompt_tool_override("Read", &options, source.clone()).string_at(""),
            Some("Model-specific complete Read instructions.")
        );
        assert_eq!(
            apply_prompt_tool_override("Write", &options, source).string_units(""),
            Some(source_units)
        );
    }

    #[test]
    fn legacy_native_selector_keeps_absent_models_long() {
        assert!(!dh_simple_system_prompt(None));
        assert!(!dh_simple_system_prompt(Some("")));
    }

    #[test]
    fn tool_selector_matches_native_319_gate_matrix() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../scripts/tests/fixtures/tool_prompts_319.json"
        ))
        .unwrap();
        for row in fixture["gateCases"].as_array().unwrap() {
            let lean = lingxi_core::host::model_capabilities::claude_lean_prompt_allowed(
                row["model"].as_str().unwrap(),
                row["capability"].as_str(),
                None,
                row["simple"].as_str(),
                false,
            );
            assert_eq!(lean, row["lean"].as_bool().unwrap(), "{row}");
        }
    }

    #[test]
    fn classic_and_lean_models_preserve_the_claude_bundle_policy() {
        for model in [
            "claude-haiku-5-5",
            "claude-sonnet-5-5",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-fable-5-1",
            "claude-mythos-5-1",
        ] {
            assert!(dh_simple_system_prompt(Some(model)), "{model}");
        }
        for model in [
            "claude-3-5-sonnet-20241022",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-opus-4-7",
            "claude-sonnet-4-5-eap",
        ] {
            assert!(!dh_simple_system_prompt(Some(model)), "{model}");
        }
    }

    #[test]
    fn claude_transports_share_the_canonical_model_policy() {
        for model in [
            "openrouter/anthropic/claude-opus-5-5",
            "us.anthropic.claude-opus-5-v1:0",
            "claude-sonnet-5-5@20261101",
        ] {
            assert!(dh_simple_system_prompt(Some(model)), "{model}");
        }
    }

    #[test]
    fn legacy_native_selector_excludes_non_claude_models() {
        for model in ["gpt-5.5", "deepseek-flash", "gemini-3.5-flash", "glm-5.1"] {
            assert!(!dh_simple_system_prompt(Some(model)), "{model}");
        }
    }
}
