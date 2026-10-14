//! `/model` — list (no arg) or switch (one arg) the active model.
//!
//! Display templates (`LingXi` UX, M5-11 T0 step 2 L3/L4):
//!   - No-arg list: the current qualified model plus provider-grouped choices
//!     (`provider/model`). Catalog-less compatibility handles retain the legacy
//!     comma-separated list.
//!   - Switch:      `"Switched to model: {name}"`
//!
//! Failure prefix: `"Could not switch model: "`.
//!
//! See plan `docs/superpowers/plans/2026-05-25-m5-11-commands-batch-2.md`
//! Task 4.

use crate::builtin_support::names::core_description;
use crate::model::{trim_source_text, BuiltinCommandHandler, CommandResult};
use crate::parser::ParsedSlashCommand;
use async_trait::async_trait;
use lingxi_core::host::{ModelListing, OrchestratorHandle};
use std::sync::Arc;
use telemetry::tengu::command as cmd_evt;

fn provider_for_ref<'a>(
    model_ref: &str,
    listings: &'a [ModelListing],
) -> Option<(&'a str, &'a str)> {
    listings
        .iter()
        .find(|listing| {
            lingxi_core::host::qualified_model_ref(
                &listing.request_model,
                Some(&listing.provider_id),
            ) == model_ref
        })
        .or_else(|| {
            let (provider, _) = model_ref.split_once('/')?;
            listings
                .iter()
                .find(|listing| listing.provider_id == provider)
        })
        .map(|listing| {
            (
                listing.provider_id.as_str(),
                listing.provider_label.as_str(),
            )
        })
}

fn render_grouped_model_refs(refs: &[String], listings: &[ModelListing]) -> String {
    if listings.is_empty() {
        return refs.join(", ");
    }

    let mut groups: Vec<(Option<String>, String, Vec<&str>)> = Vec::new();
    for model_ref in refs {
        let provider = provider_for_ref(model_ref, listings);
        let key = provider.map(|(id, _)| id.to_string());
        let label = provider
            .map(|(id, label)| {
                if label.is_empty() {
                    id.to_string()
                } else {
                    label.to_string()
                }
            })
            .unwrap_or_else(|| "Other".to_string());
        if let Some((_, _, models)) = groups.iter_mut().find(|(group, _, _)| *group == key) {
            models.push(model_ref);
        } else {
            groups.push((key, label, vec![model_ref]));
        }
    }

    if groups.is_empty() {
        return "(none)".to_string();
    }

    let mut lines = Vec::new();
    for (_, label, models) in groups {
        lines.push(format!("{label}:"));
        lines.extend(models.into_iter().map(|model| format!("  - {model}")));
    }
    lines.join("\n")
}

/// `/model` handler — list / switch active model.
#[derive(Clone)]
pub struct ModelHandler {
    handle: Arc<dyn OrchestratorHandle>,
}

impl ModelHandler {
    /// Construct a `ModelHandler` bound to the given orchestrator handle.
    #[must_use]
    pub fn new(handle: Arc<dyn OrchestratorHandle>) -> Self {
        Self { handle }
    }
}

#[async_trait]
impl BuiltinCommandHandler for ModelHandler {
    async fn handle(&self, args: &ParsedSlashCommand) -> CommandResult {
        telemetry::emit_command_started(cmd_evt::MODEL_STARTED);
        let model_selection = trim_source_text(&args.raw_args);
        let trimmed = model_selection.string_at("").unwrap_or_default();
        if trimmed.is_empty() {
            // List mode. Curate to the "latest few" per provider (the shared
            // `lingxi_core::host::curated_model_refs`, same whitelist as the TUI picker +
            // mobile listing) instead of joining the full assembled catalog
            // (~hundreds of ids — every preset is injected into the live config by
            // `provider_config::assemble`). Qualified refs preserve provider
            // identity when two providers expose the same wire model id. With no
            // catalog wired (library/stub), the raw legacy list remains unchanged.
            let available = self.handle.list_available_models().await;
            let listings = self.handle.list_model_listings().await;
            let snap = self.handle.get_status_snapshot().await;
            let models = lingxi_core::host::curated_model_refs(
                &listings,
                &available,
                &snap.model,
                snap.model_profile.as_deref(),
            );
            // The helper keeps the active model first and can infer its unique
            // provider when an older status snapshot omits `model_profile`.
            // Reuse that exact ref so Current and the selectable row agree.
            let current = snap
                .model_source
                .as_ref()
                .and_then(|source| source.string_units(""))
                .map(|units| String::from_utf16_lossy(&units))
                .unwrap_or_else(|| {
                    if snap.model.is_empty() {
                        String::new()
                    } else {
                        models.first().cloned().unwrap_or_else(|| {
                            lingxi_core::host::qualified_model_ref(
                                &snap.model,
                                snap.model_profile.as_deref(),
                            )
                        })
                    }
                });
            let available = render_grouped_model_refs(&models, &listings);
            let available = if listings.is_empty() {
                format!("Available: {available}")
            } else {
                format!("Available:\n{available}")
            };
            telemetry::emit_command_completed(cmd_evt::MODEL_COMPLETED, "list");
            return CommandResult::Done {
                display: Some((format!("Current model: {current}\n{available}")).into()),
            };
        }
        // Switch mode. Resolve an optional `profile/model` qualifier so a shared
        // id (offered by multiple providers) routes deterministically.
        let listings = self.handle.list_model_listings().await;
        let (model, profile) = lingxi_core::host::parse_model_ref(trimmed, &listings);
        match self
            .handle
            .select_model(lingxi_core::host::ModelSelectionRequest::Select {
                source: lingxi_core::types::utf16_json::Utf16JsonProjection::from(model.clone()),
                route_hint: profile
                    .as_deref()
                    .map(|profile| lingxi_core::host::ModelRouteAnchor {
                        model: model.clone(),
                        profile: Some(profile.to_owned()),
                    }),
                origin: lingxi_core::host::ModelSelectionOrigin::Command,
                validation: lingxi_core::host::ModelValidationMode::SlashOuterRaceDetached,
            })
            .await
        {
            Ok(()) => {
                telemetry::emit_command_completed(cmd_evt::MODEL_COMPLETED, "switch");
                let status = self.handle.get_status_snapshot().await;
                let selected = status
                    .model_source
                    .as_ref()
                    .and_then(|source| source.string_units(""))
                    .map(|units| String::from_utf16_lossy(&units))
                    .filter(|source| !source.is_empty())
                    .unwrap_or_else(|| {
                        if status.model.is_empty() {
                            model.clone()
                        } else {
                            status.model
                        }
                    });
                CommandResult::Done {
                    display: Some((format!("Switched to model: {selected}")).into()),
                }
            }
            Err(e) => {
                let msg = e.to_string();
                telemetry::emit_command_failed(cmd_evt::MODEL_FAILED, &msg);
                CommandResult::Done {
                    display: Some((format!("Could not switch model: {msg}")).into()),
                }
            }
        }
    }
    fn name(&self) -> &str {
        "model"
    }
    fn description(&self) -> &str {
        core_description("model")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SourceTextTestExt;
    use lingxi_core::types::utf16_json::Utf16JsonProjection;
    use orchestrator::test_support::MockOrchestratorHandle;

    fn args(raw: &str) -> ParsedSlashCommand {
        ParsedSlashCommand {
            name: "model".into(),
            raw_args: raw.into(),
            positional_args: raw.split_whitespace().map(Into::into).collect(),
        }
    }

    #[tokio::test]
    async fn list_mode_when_no_args() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_available_models(vec!["claude-opus-4-7".into(), "claude-sonnet-4-6".into()]);
        let snap = lingxi_core::host::StatusSnapshot {
            model: "claude-opus-4-7".into(),
            model_source: None,
            ..lingxi_core::host::StatusSnapshot::default()
        };
        mock.set_status_snapshot(snap);
        let h = ModelHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("")).await {
            assert_eq!(
                s,
                "Current model: claude-opus-4-7\nAvailable: claude-opus-4-7, claude-sonnet-4-6"
                    .into()
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn list_mode_displays_logical_model_source_over_its_route_anchor() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_available_models(vec!["claude-known".into()]);
        let source_units = "claude-custom-"
            .encode_utf16()
            .chain([0xd800])
            .collect::<Vec<_>>();
        mock.set_status_snapshot(lingxi_core::host::StatusSnapshot {
            model: "claude-known".into(),
            model_source: Some(
                Utf16JsonProjection::root_string(
                    String::from_utf16_lossy(&source_units),
                    source_units,
                )
                .expect("exact source projection"),
            ),
            ..lingxi_core::host::StatusSnapshot::default()
        });

        let handler = ModelHandler::new(mock);
        let CommandResult::Done {
            display: Some(display),
        } = handler.handle(&args("")).await
        else {
            panic!();
        };
        assert!(display.starts_with("Current model: claude-custom-�\n"));
    }

    #[tokio::test]
    async fn list_mode_groups_curated_qualified_refs_by_provider() {
        fn listing(provider_id: &str, provider_label: &str, request_model: &str) -> ModelListing {
            ModelListing {
                display_model: request_model.to_string(),
                request_model: request_model.to_string(),
                provider_id: provider_id.to_string(),
                provider_label: provider_label.to_string(),
                description: None,
                metadata: Default::default(),
                capabilities: Default::default(),
                reasoning: Default::default(),
                supports_reasoning: true,
                fusion_analyst_capable: false,
                connection: Default::default(),
            }
        }

        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_available_models(vec!["gpt-5.6-sol".into(), "gpt-4o".into()]);
        mock.set_model_listings(vec![
            listing("openai", "OpenAI", "gpt-5.6-sol"),
            listing("openai", "OpenAI", "gpt-5.6-terra"),
            listing("openai", "OpenAI", "gpt-4o"),
            listing("github-copilot", "GitHub Copilot", "gpt-5.6-sol"),
        ]);
        mock.set_status_snapshot(lingxi_core::host::StatusSnapshot {
            model: "gpt-5.6-sol".into(),
            model_source: None,
            model_profile: Some("github-copilot".into()),
            ..lingxi_core::host::StatusSnapshot::default()
        });

        let h = ModelHandler::new(mock);
        let CommandResult::Done { display: Some(s) } = h.handle(&args("")).await else {
            panic!();
        };

        assert_eq!(
            s,
            Utf16JsonProjection::from(concat!(
                "Current model: github-copilot/gpt-5.6-sol\n",
                "Available:\n",
                "GitHub Copilot:\n",
                "  - github-copilot/gpt-5.6-sol\n",
                "OpenAI:\n",
                "  - openai/gpt-5.6-sol\n",
                "  - openai/gpt-5.6-terra"
            ))
        );
        assert!(!s.contains("gpt-4o"), "non-curated models stay hidden");
    }

    #[tokio::test]
    async fn list_mode_uses_inferred_provider_for_current_model() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_model_listings(vec![ModelListing {
            display_model: "DeepSeek V4.1 Flash".into(),
            request_model: "deepseek-flash".into(),
            provider_id: "deepseek".into(),
            provider_label: "DeepSeek".into(),
            description: None,
            metadata: Default::default(),
            capabilities: Default::default(),
            reasoning: Default::default(),
            supports_reasoning: true,
            fusion_analyst_capable: false,
            connection: Default::default(),
        }]);
        mock.set_status_snapshot(lingxi_core::host::StatusSnapshot {
            model: "deepseek-flash".into(),
            model_source: None,
            model_profile: None,
            ..lingxi_core::host::StatusSnapshot::default()
        });

        let h = ModelHandler::new(mock);
        let CommandResult::Done { display: Some(s) } = h.handle(&args("")).await else {
            panic!();
        };
        assert!(s.starts_with("Current model: deepseek/deepseek-flash\n"));
        assert!(s.contains("  - deepseek/deepseek-flash"));
    }

    #[tokio::test]
    async fn switch_mode_when_arg_provided() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ModelHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("claude-haiku-4-5")).await
        {
            assert_eq!(s, "Switched to model: claude-haiku-4-5".into());
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn switch_mode_with_whitespace_trim() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ModelHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } =
            h.handle(&args("  claude-opus-4-7  ")).await
        {
            assert_eq!(s, "Switched to model: claude-opus-4-7".into());
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn switch_mode_forwards_exact_utf16_source_even_when_route_text_is_unpaired() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let handler = ModelHandler::new(mock.clone());
        let units = "/model ".encode_utf16().chain([0xD800]).collect::<Vec<_>>();
        let raw = Utf16JsonProjection::root_string(String::from_utf16_lossy(&units), units)
            .expect("slash input source projection");
        let parsed = crate::parse_slash_command(&raw).expect("model command parse");
        let selection = parsed
            .raw_args
            .string_units("")
            .expect("model command argument is a source string");

        let result = handler.handle(&parsed).await;

        assert!(matches!(result, CommandResult::Done { .. }));
        assert_eq!(
            mock.last_switch_model_selection()
                .and_then(|source| source.string_units("")),
            Some(selection),
            "route display replacement must not replace the Native selection source"
        );
    }

    #[tokio::test]
    async fn switch_failure_prefixes_error() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_switch_model_error("invalid name".into());
        let h = ModelHandler::new(mock);
        if let CommandResult::Done { display: Some(s) } = h.handle(&args("foo")).await {
            assert_eq!(
                s,
                "Could not switch model: handle action failed: invalid name".into()
            );
        } else {
            panic!();
        }
    }

    #[tokio::test]
    async fn name_and_description() {
        let mock = Arc::new(MockOrchestratorHandle::new());
        let h = ModelHandler::new(mock);
        assert_eq!(h.name(), "model");
        assert_eq!(h.description(), "Set the AI model for LingXi");
    }

    /// `/model openai/gpt-5.2` with a fixture that has both openai and
    /// github-copilot offering `gpt-5.2` must pass `("gpt-5.2", Some("openai"))`
    /// to `switch_model`. A model unique to one provider (`gpt-4.1`) must pass
    /// `("gpt-4.1", None)`.
    #[tokio::test]
    async fn model_switch_parses_profile_qualified_ref() {
        use lingxi_core::host::ModelListing;
        fn listing(provider_id: &str, request_model: &str) -> ModelListing {
            ModelListing {
                display_model: request_model.to_string(),
                request_model: request_model.to_string(),
                provider_id: provider_id.to_string(),
                provider_label: provider_id.to_string(),
                description: None,
                metadata: Default::default(),
                capabilities: Default::default(),
                reasoning: Default::default(),
                supports_reasoning: false,
                fusion_analyst_capable: false,
                connection: Default::default(),
            }
        }

        let mock = Arc::new(MockOrchestratorHandle::new());
        mock.set_model_listings(vec![
            listing("openai", "gpt-5.2"),
            listing("github-copilot", "gpt-5.2"),
            listing("openai", "gpt-4.1"),
        ]);
        let h = ModelHandler::new(mock.clone());

        // Qualified ref resolves to (bare model, profile).
        let _ = h.handle(&args("openai/gpt-5.2")).await;
        assert_eq!(
            mock.last_switch(),
            Some(("gpt-5.2".to_string(), Some("openai".to_string())))
        );

        // Unique bare model → no profile.
        let _ = h.handle(&args("gpt-4.1")).await;
        assert_eq!(mock.last_switch(), Some(("gpt-4.1".to_string(), None)));
    }
}
