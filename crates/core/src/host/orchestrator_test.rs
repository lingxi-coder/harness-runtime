//! Tests for `orchestrator.rs`, extracted from inline `#[cfg(test)]` blocks. Included via `#[path] mod orchestrator_test;`.

use super::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn _trait_objects_compile() {
        fn _f<T: OutputStream + 'static>(t: T) -> Box<dyn OutputStream> {
            Box::new(t)
        }
        fn _g<T: OrchestratorHandle + 'static>(t: T) -> Box<dyn OrchestratorHandle> {
            Box::new(t)
        }
    }

    #[test]
    fn output_event_round_trips_through_json() {
        let ev = OutputEvent::Text {
            text: "hello".into(),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    /// Coordinator-activation T08: the additive `emit_coordinator_status`
    /// PUSH hook ships as a default no-op so every pre-existing `OutputStream`
    /// impl (TUI / CLI / `MockOutputStream`) keeps compiling without an
    /// override. This bare unit struct implements ONLY the four required
    /// methods and relies on the default for `emit_coordinator_status`;
    /// driving it must neither fail to compile nor panic, for both a
    /// `Some(team)` and a `None` team.
    #[tokio::test]
    async fn emit_coordinator_status_default_is_noop() {
        struct BareSink;

        #[async_trait]
        impl OutputStream for BareSink {
            async fn emit_text(&self, _text: &str) {}
            async fn emit_tool_call(
                &self,
                _id: &crate::types::ToolUseId,
                _tool: &str,
                _input: &serde_json::Value,
            ) {
            }
            async fn emit_tool_result(
                &self,
                _id: &crate::types::ToolUseId,
                _tool: &str,
                _model_text: &str,
                _result: &serde_json::Value,
            ) {
            }
            async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
        }

        // Object-safe behind `dyn` (matches how engines hold it).
        let sink: Box<dyn OutputStream> = Box::new(BareSink);
        // The default no-op must simply return for both team shapes.
        sink.emit_coordinator_status(3, Some("alpha")).await;
        sink.emit_coordinator_status(0, None).await;
    }

    /// M6-04 Task 1: `OutputEvent::ToolCall` must carry a `ToolUseId` so the
    /// TUI can correlate calls with their results and key the per-tool
    /// expanded-state map.
    #[test]
    fn output_event_tool_call_carries_tool_use_id() {
        use crate::types::ToolUseId;
        let id = ToolUseId::new();
        let ev = OutputEvent::ToolCall {
            id,
            tool: "Read".into(),
            input: serde_json::json!({"file_path": "/tmp/x.rs"}),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    /// M6-04 Task 1: same for `OutputEvent::ToolResult`.
    #[test]
    fn output_event_tool_result_carries_tool_use_id() {
        use crate::types::ToolUseId;
        let id = ToolUseId::new();
        let ev = OutputEvent::ToolResult {
            id,
            tool: "Read".into(),
            result: serde_json::json!({"content": "fn main() {}"}),
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);
    }

    #[test]
    fn cost_snapshot_default_is_zero() {
        let s = CostSnapshot::default();
        assert_eq!(s.total_nano_usd, 0);
        assert_eq!(s.total_tokens, 0);
        assert!((s.total_usd - 0.0).abs() < f64::EPSILON);
        assert_eq!(s.api_calls, 0);
        assert_eq!(s.session_duration, std::time::Duration::ZERO);
    }

    #[test]
    fn compaction_summary_default_is_zero() {
        let s = CompactionSummary::default();
        assert_eq!(s.messages_before, 0);
        assert_eq!(s.messages_after, 0);
        assert_eq!(s.bytes_saved, 0);
    }

    // M5-11 trait extension tests

    #[allow(dead_code)]
    fn _handle_remains_object_safe_after_m5_11() {
        let _: Option<Box<dyn OrchestratorHandle>> = None;
    }

    #[test]
    fn mcp_server_info_fields() {
        let info = McpServerInfo {
            name: "memory".to_string(),
            status: McpStatus::Connected,
            transport: "stdio".to_string(),
        };
        assert_eq!(info.name, "memory");
        assert!(matches!(info.status, McpStatus::Connected));
        assert_eq!(info.transport, "stdio");
    }

    #[test]
    fn hook_info_fields() {
        let info = HookInfo {
            name: "fmt-on-write".to_string(),
            event: "PostToolUse".to_string(),
            matcher: Some("Write|Edit".to_string()),
            timeout_ms: 60_000,
            ..HookInfo::default()
        };
        assert_eq!(info.timeout_ms, 60_000);
        assert_eq!(info.matcher.as_deref(), Some("Write|Edit"));
    }

    #[test]
    fn agent_info_fields() {
        let info = AgentInfo {
            name: "reviewer".to_string(),
            description: "review code".to_string(),
            tools_allowed: vec!["Read".to_string(), "Grep".to_string()],
            wildcard_tools: false,
            ..AgentInfo::default()
        };
        assert_eq!(info.tools_allowed.len(), 2);
    }

    #[test]
    fn doctor_report_default_is_empty() {
        let r = DoctorReport::default();
        assert_eq!(r.checks.len(), 0);
        assert_eq!(r.summary.passed, 0);
        assert_eq!(r.summary.warnings, 0);
        assert_eq!(r.summary.failed, 0);
    }

    #[test]
    fn status_snapshot_default_is_zero() {
        let s = StatusSnapshot::default();
        assert_eq!(s.n_messages, 0);
        assert_eq!(s.n_mcp_connected, 0);
        assert_eq!(s.n_mcp_total, 0);
        assert_eq!(s.n_hooks, 0);
        assert_eq!(s.n_agents, 0);
    }

    #[test]
    fn check_status_variants() {
        let pass = CheckStatus::Pass;
        let warn = CheckStatus::Warn;
        let fail = CheckStatus::Fail;
        assert_ne!(pass, warn);
        assert_ne!(warn, fail);
    }

    // ── RateLimitSnapshot ─────────────────────────────────────────────────────

    /// `RateLimitSnapshot::default()` has all three fields `None`.
    #[test]
    fn rate_limit_snapshot_default_all_none() {
        let s = RateLimitSnapshot::default();
        assert!(s.rate_limit_type.is_none());
        assert!(s.overage_status.is_none());
        assert!(s.overage_disabled_reason.is_none());
    }

    /// A fully-populated snapshot round-trips through equality checks correctly.
    #[test]
    fn rate_limit_snapshot_fields_round_trip() {
        let s = RateLimitSnapshot {
            rate_limit_type: Some("five_hour".to_string()),
            overage_status: Some("allowed_warning".to_string()),
            overage_disabled_reason: Some("out_of_credits".to_string()),
        };
        assert_eq!(s.rate_limit_type.as_deref(), Some("five_hour"));
        assert_eq!(s.overage_status.as_deref(), Some("allowed_warning"));
        assert_eq!(
            s.overage_disabled_reason.as_deref(),
            Some("out_of_credits"),
            "overage_disabled_reason must round-trip"
        );
    }

    /// Two snapshots with identical fields compare equal (`PartialEq`).
    #[test]
    fn rate_limit_snapshot_eq() {
        let a = RateLimitSnapshot {
            rate_limit_type: Some("seven_day".to_string()),
            overage_status: Some("rejected".to_string()),
            overage_disabled_reason: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
        let c = RateLimitSnapshot {
            overage_disabled_reason: Some("out_of_credits".to_string()),
            ..b
        };
        assert_ne!(
            a, c,
            "different overage_disabled_reason must compare unequal"
        );
    }

    /// The default impl of `last_rate_limit_info` on `OrchestratorHandle`
    /// returns `None` — ensures implementors that don't override get a safe
    /// default.
    #[tokio::test]
    async fn last_rate_limit_info_default_is_none() {
        struct MinimalHandle;

        #[async_trait]
        impl OrchestratorHandle for MinimalHandle {
            async fn current_session_id(&self) -> SessionId {
                SessionId::new()
            }
            async fn clear_session(&self) -> Result<(), HandleError> {
                Ok(())
            }
            async fn force_compact(&self) -> Result<CompactionSummary, HandleError> {
                Ok(CompactionSummary::default())
            }
            async fn snapshot_cost(&self) -> CostSnapshot {
                CostSnapshot::default()
            }
            async fn switch_model(&self, _: &str, _: Option<&str>) -> Result<(), HandleError> {
                Ok(())
            }
            async fn request_exit(&self) {}
            async fn current_should_exit(&self) -> bool {
                false
            }
            async fn open_memory_editor(&self) -> Result<MemoryEditorOutcome, HandleError> {
                Ok(MemoryEditorOutcome {
                    edited_path: PathBuf::new(),
                    exit_code: 0,
                })
            }
            async fn list_mcp_servers(&self) -> Vec<McpServerInfo> {
                Vec::new()
            }
            async fn list_skills(&self) -> Vec<SkillInfo> {
                Vec::new()
            }
            async fn list_hooks(&self) -> Vec<HookInfo> {
                Vec::new()
            }
            async fn list_agents(&self) -> Vec<AgentInfo> {
                Vec::new()
            }
            async fn run_doctor_checks(&self) -> DoctorReport {
                DoctorReport::default()
            }
            async fn get_status_snapshot(&self) -> StatusSnapshot {
                StatusSnapshot::default()
            }
            async fn edit_config_file(&self) -> Result<MemoryEditorOutcome, HandleError> {
                Ok(MemoryEditorOutcome {
                    edited_path: PathBuf::new(),
                    exit_code: 0,
                })
            }
            async fn edit_permissions_file(&self) -> Result<MemoryEditorOutcome, HandleError> {
                Ok(MemoryEditorOutcome {
                    edited_path: PathBuf::new(),
                    exit_code: 0,
                })
            }
            async fn list_available_models(&self) -> Vec<String> {
                Vec::new()
            }
        }

        let h = MinimalHandle;
        assert!(
            h.last_rate_limit_info().await.is_none(),
            "default impl must return None"
        );
    }

    // ── OutputEvent::RateLimit (llm-runtime future-work batch 3, Task 8) ──────

    /// The additive `RateLimit` variant round-trips through the enum's
    /// default serde conventions (externally tagged, named struct fields),
    /// both fully populated and all-`None`.
    #[test]
    fn output_event_rate_limit_round_trips_through_json() {
        let ev = OutputEvent::RateLimit {
            status: Some("allowed_warning".to_string()),
            rate_limit_type: Some("five_hour".to_string()),
            utilization: Some(0.85),
            resets_at: Some(1_760_000_000),
            claim_resets_at: Some(1_760_000_100),
            overage_status: Some("allowed".to_string()),
            overage_resets_at: Some(1_760_000_200),
            overage_disabled_reason: Some("out_of_credits".to_string()),
            fallback_available: Some(true),
            upgrade_paths: Some(vec!["overage".to_string()]),
            credits_required: true,
        };
        let s = serde_json::to_string(&ev).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);

        let empty = OutputEvent::RateLimit {
            status: None,
            rate_limit_type: None,
            utilization: None,
            resets_at: None,
            claim_resets_at: None,
            overage_status: None,
            overage_resets_at: None,
            overage_disabled_reason: None,
            fallback_available: None,
            upgrade_paths: None,
            credits_required: false,
        };
        let s = serde_json::to_string(&empty).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(empty, back);
    }

    /// Task 8: the additive `emit_rate_limit` PUSH hook ships as a default
    /// no-op so every pre-existing `OutputStream` impl keeps compiling
    /// without an override. A bare sink implementing only the four required
    /// methods must accept the call (populated and all-`None`) and return.
    #[tokio::test]
    async fn emit_rate_limit_default_is_noop() {
        struct BareSink;

        #[async_trait]
        impl OutputStream for BareSink {
            async fn emit_text(&self, _text: &str) {}
            async fn emit_tool_call(
                &self,
                _id: &crate::types::ToolUseId,
                _tool: &str,
                _input: &serde_json::Value,
            ) {
            }
            async fn emit_tool_result(
                &self,
                _id: &crate::types::ToolUseId,
                _tool: &str,
                _model_text: &str,
                _result: &serde_json::Value,
            ) {
            }
            async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
        }

        // Object-safe behind `dyn` (matches how the orchestrator holds it).
        let sink: Box<dyn OutputStream> = Box::new(BareSink);
        sink.emit_rate_limit(
            Some("allowed_warning"),
            Some("five_hour"),
            Some(0.85),
            Some(1_760_000_000),
            Some(1_760_000_100),
            Some("allowed"),
            Some(1_760_000_200),
            Some("out_of_credits"),
            Some(true),
            Some(&["overage".to_string()]),
            true,
        )
        .await;
        sink.emit_rate_limit(
            None, None, None, None, None, None, None, None, None, None, false,
        )
        .await;
    }

    // ── OutputEvent::RawUtilization (llm-runtime future-work batch 5, Task 1) ─

    /// The additive `RawUtilization` variant constructs and round-trips
    /// through the enum's default serde conventions (externally tagged,
    /// named struct fields), both fully populated and all-`None`.
    #[test]
    fn raw_utilization_variant_constructs() {
        let ev = OutputEvent::RawUtilization {
            five_hour_utilization: Some(0.42),
            five_hour_resets_at: Some(1_760_000_000),
            seven_day_utilization: Some(0.9),
            seven_day_resets_at: Some(1_760_500_000),
        };
        assert!(matches!(ev, OutputEvent::RawUtilization { .. }));
        let s = serde_json::to_string(&ev).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(ev, back);

        let empty = OutputEvent::RawUtilization {
            five_hour_utilization: None,
            five_hour_resets_at: None,
            seven_day_utilization: None,
            seven_day_resets_at: None,
        };
        let s = serde_json::to_string(&empty).unwrap();
        let back: OutputEvent = serde_json::from_str(&s).unwrap();
        assert_eq!(empty, back);
    }

    /// Batch 5, Task 1: the additive `emit_raw_utilization` PUSH hook ships
    /// as a default no-op so every pre-existing `OutputStream` impl keeps
    /// compiling without an override. A bare sink implementing only the four
    /// required methods must accept the call (populated and all-`None`) and
    /// return.
    #[tokio::test]
    async fn emit_raw_utilization_default_is_noop() {
        struct BareSink;

        #[async_trait]
        impl OutputStream for BareSink {
            async fn emit_text(&self, _text: &str) {}
            async fn emit_tool_call(
                &self,
                _id: &crate::types::ToolUseId,
                _tool: &str,
                _input: &serde_json::Value,
            ) {
            }
            async fn emit_tool_result(
                &self,
                _id: &crate::types::ToolUseId,
                _tool: &str,
                _model_text: &str,
                _result: &serde_json::Value,
            ) {
            }
            async fn emit_end_turn(&self, _stop_reason: &str, _cost: &CostSnapshot) {}
        }

        // Object-safe behind `dyn` (matches how the orchestrator holds it).
        let sink: Box<dyn OutputStream> = Box::new(BareSink);
        sink.emit_raw_utilization(
            Some(0.42),
            Some(1_760_000_000),
            Some(0.9),
            Some(1_760_500_000),
        )
        .await;
        sink.emit_raw_utilization(None, None, None, None).await;
    }
}

#[cfg(test)]
mod provider_boot_default_tests {
    use super::{
        is_curated_model, provider_default_model, provider_fallback_order,
        provider_has_curated_list,
    };

    /// Every provider in the fallback order has a boot-default model, and — for
    /// providers with a curated shortlist — that default is itself curated
    /// (single source of truth with [`is_curated_model`]).
    #[test]
    fn defaults_exist_and_are_curated() {
        for p in provider_fallback_order() {
            let m = provider_default_model(p).expect("ordered provider has a default");
            if provider_has_curated_list(p) {
                assert!(is_curated_model(p, m), "{p}/{m} must be curated");
            }
        }
    }

    #[test]
    fn order_is_anthropic_first_unique_and_covers_openrouter() {
        let order = provider_fallback_order();
        assert_eq!(order.first(), Some(&"anthropic"));
        let set: std::collections::HashSet<_> = order.iter().collect();
        assert_eq!(set.len(), order.len(), "no duplicate providers");
        assert!(
            order.contains(&"openrouter"),
            "aggregator last-resort present"
        );
        assert_eq!(order.last(), Some(&"openrouter"), "aggregator ranks last");
    }

    /// OpenRouter has no curated shortlist; its boot default is the `auto`
    /// meta-router rather than an arbitrary alphabetical pick.
    #[test]
    fn openrouter_default_is_the_auto_router() {
        assert_eq!(
            provider_default_model("openrouter"),
            Some("openrouter/auto")
        );
    }

    /// The `builtin` alias mirrors anthropic (same as [`is_curated_model`]).
    #[test]
    fn builtin_alias_mirrors_anthropic() {
        assert_eq!(
            provider_default_model("builtin"),
            provider_default_model("anthropic")
        );
    }

    #[test]
    fn unknown_provider_has_no_default() {
        assert_eq!(provider_default_model("groq"), None);
        assert_eq!(provider_default_model(""), None);
    }

    #[test]
    fn deepseek_defaults_to_current_v4_flash_and_hides_retired_ids() {
        assert_eq!(provider_default_model("deepseek"), Some("deepseek-flash"));
        assert!(is_curated_model("deepseek", "deepseek-flash"));
        assert!(is_curated_model("deepseek", "deepseek-v4-pro"));
        assert!(!is_curated_model("deepseek", "deepseek-chat"));
        assert!(!is_curated_model("deepseek", "deepseek-reasoner"));
    }

    #[test]
    fn latest_shortlists_drop_older_provider_models() {
        assert!(is_curated_model("openai", "gpt-5.6-sol"));
        assert!(is_curated_model("openai", "gpt-5.6-terra"));
        assert!(is_curated_model("openai", "gpt-5.6-luna"));
        assert!(!is_curated_model("openai", "gpt-5.5"));
        assert!(is_curated_model("openai-chatgpt", "gpt-5.6-sol"));
        assert!(!is_curated_model("openai-chatgpt", "gpt-5.3-codex"));
        assert!(is_curated_model("anthropic", "claude-opus-5"));
        assert!(is_curated_model("anthropic", "claude-fable-5-1"));
        assert!(!is_curated_model("anthropic", "claude-fable-5"));
        assert!(!is_curated_model("anthropic", "claude-opus-4-8"));
        assert!(is_curated_model("glm-coding", "glm-5.3"));
        assert!(is_curated_model("zai", "glm-5.3"));
        assert!(is_curated_model("zai", "glm-5.3-flash"));
        assert!(is_curated_model("kimi", "kimi-k3"));
        assert!(is_curated_model("kimi-code", "k3"));
    }

    #[test]
    fn openrouter_curates_latest_aliases_and_top_free_models() {
        for latest in [
            "~anthropic/claude-fable-latest",
            "~openai/gpt-latest",
            "~google/gemini-pro-latest",
            "~deepseek/deepseek-v4-flash-latest",
            "openai/gpt-chat-latest",
        ] {
            assert!(is_curated_model("openrouter", latest), "missing {latest}");
        }
        for free in [
            "openrouter/free",
            "cohere/north-mini-code:free",
            "z-ai/glm-5.2:free",
            "thinkingmachines/inkling:free",
            "thinkingmachines/inkling-small:free",
            "minimax/minimax-m3:free",
            "minimax/minimax-m2.7:free",
            "poolside/laguna-s-2.1:free",
            "inclusionai/ling-3.0-flash-fin:free",
            "nvidia/nemotron-3.5-lightning:free",
            "nvidia/nemotron-3-ultra-550b-a55b:free",
        ] {
            assert!(is_curated_model("openrouter", free), "missing {free}");
        }
        assert!(!is_curated_model("openrouter", "liquid/lfm-2.5-2.6b:free"));
    }

    #[test]
    fn kimi_profiles_use_accessible_defaults_and_curate_current_agent_models() {
        assert_eq!(provider_default_model("kimi"), Some("kimi-k3"));
        assert!(is_curated_model("kimi", "kimi-k3"));
        assert!(!is_curated_model("kimi", "kimi-k2.7-code"));
        assert!(!is_curated_model("kimi", "kimi-k2.6"));
        assert!(!is_curated_model("kimi", "kimi-k2-thinking"));
        assert_eq!(provider_default_model("kimi-code"), Some("k3"));
        assert!(is_curated_model("kimi-code", "k3"));
        assert!(!is_curated_model("kimi-code", "kimi-for-coding"));
    }
}

#[cfg(test)]
mod parse_model_ref_tests {
    use super::{parse_model_ref, ModelListing};
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
    fn fixture() -> Vec<ModelListing> {
        vec![
            listing("openai", "gpt-5.2"),
            listing("openai", "gpt-4o"),
            listing("github-copilot", "gpt-5.2"),
            listing("openrouter", "openai/gpt-4o"),
        ]
    }
    #[test]
    fn bare_id_no_slash() {
        assert_eq!(
            parse_model_ref("gpt-5.2", &fixture()),
            ("gpt-5.2".into(), None)
        );
    }
    #[test]
    fn qualified_two_segments() {
        assert_eq!(
            parse_model_ref("openai/gpt-5.2", &fixture()),
            ("gpt-5.2".into(), Some("openai".into()))
        );
        assert_eq!(
            parse_model_ref("github-copilot/gpt-5.2", &fixture()),
            ("gpt-5.2".into(), Some("github-copilot".into()))
        );
    }
    #[test]
    fn two_segment_prefers_qualified_when_model_in_profile() {
        assert_eq!(
            parse_model_ref("openai/gpt-4o", &fixture()),
            ("gpt-4o".into(), Some("openai".into()))
        );
    }
    #[test]
    fn fully_qualified_openrouter_slash_id() {
        assert_eq!(
            parse_model_ref("openrouter/openai/gpt-4o", &fixture()),
            ("openai/gpt-4o".into(), Some("openrouter".into()))
        );
    }
    #[test]
    fn unknown_prefix_is_bare() {
        assert_eq!(
            parse_model_ref("foo/bar", &fixture()),
            ("foo/bar".into(), None)
        );
    }
    #[test]
    fn degenerate_inputs_safe() {
        assert_eq!(parse_model_ref("", &fixture()), ("".into(), None));
        assert_eq!(parse_model_ref("/", &fixture()), ("/".into(), None));
    }
}

#[cfg(test)]
mod reasoning_controls_tests {
    use super::{
        reasoning_control_spec_for_model, validated_reasoning_selection_for_model,
        ReasoningSelection,
    };

    #[test]
    fn unknown_models_are_auto_only() {
        let spec = reasoning_control_spec_for_model("future-model", Some("openai"));
        assert_eq!(spec.available, vec![ReasoningSelection::Automatic]);
        assert_eq!(
            validated_reasoning_selection_for_model(
                &ReasoningSelection::Level { id: "high".into() },
                "future-model",
                Some("openai"),
            ),
            ReasoningSelection::Automatic
        );
    }

    #[test]
    fn forced_reasoning_models_report_effective_provider_default() {
        let spec = reasoning_control_spec_for_model("deepseek-reasoner", Some("deepseek"));
        assert!(spec.forced);
        assert!(!spec.modifiable);
        assert_eq!(spec.provider_default, ReasoningSelection::Enabled);
        assert_eq!(
            validated_reasoning_selection_for_model(
                &ReasoningSelection::Automatic,
                "deepseek-reasoner",
                Some("deepseek"),
            ),
            ReasoningSelection::Enabled
        );
    }

    #[test]
    fn gemini_25_exposes_budget_bounds_without_inventing_levels() {
        let spec = reasoning_control_spec_for_model("gemini-2.5-flash", Some("gemini"));
        assert_eq!(
            spec.available,
            vec![ReasoningSelection::Automatic, ReasoningSelection::Disabled]
        );
        let range = spec.budget_range.expect("Gemini 2.5 budget range");
        assert_eq!((range.min_tokens, range.max_tokens), (0, 24_576));
        assert!(spec
            .available
            .iter()
            .all(|selection| { !matches!(selection, ReasoningSelection::Level { .. }) }));
    }
}

#[cfg(test)]
mod curated_model_tests {
    use super::{
        curated_model_listings, curated_model_names, curated_model_refs, is_curated_model,
        provider_model_catalog, provider_model_catalog_listings, qualified_model_ref, ModelListing,
    };

    fn listing(provider_id: &str, request_model: &str, display: &str) -> ModelListing {
        ModelListing {
            display_model: display.to_string(),
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

    fn chat_listing(provider_id: &str, request_model: &str, display: &str) -> ModelListing {
        let mut listing = listing(provider_id, request_model, display);
        listing.capabilities.tools = true;
        listing
    }

    #[test]
    fn glm_coding_keyed_on_profile_name_not_filename() {
        assert!(is_curated_model("glm-coding", "glm-5.3"));
        assert!(!is_curated_model("glm-coding", "glm-5.2"));
        assert!(!is_curated_model("zhipuai-coding-plan", "glm-5.3"));
    }

    /// A user-defined provider (a proxy, a self-hosted endpoint, any profile the
    /// shared catalog has never heard of) has no curated shortlist to trim to,
    /// so trimming to one hid EVERY model it offers. `provider_has_curated_list`
    /// exists to name exactly this case; the curation helpers now consult it,
    /// the way the TUI's row builder always has.
    #[test]
    fn a_provider_without_a_curated_shortlist_keeps_its_own_catalog() {
        let listings = vec![
            listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol"),
            listing("openai", "gpt-4o", "GPT-4o"), // curated provider → trimmed
            listing("my-proxy", "llama-3.3-70b", "Llama 3.3 70B"),
            listing("my-proxy", "some-internal-model", "Internal"),
        ];

        let refs = curated_model_refs(&listings, &[], "", None);
        assert_eq!(
            refs,
            vec![
                "openai/gpt-5.6-sol".to_string(),
                "my-proxy/llama-3.3-70b".to_string(),
                "my-proxy/some-internal-model".to_string(),
            ],
            "a provider with no shortlist must keep every model it declares"
        );

        let names = curated_model_names(&listings, &[], "");
        assert_eq!(
            names,
            vec![
                "GPT-5.6 Sol".to_string(),
                "Llama 3.3 70B".to_string(),
                "Internal".to_string(),
            ]
        );
    }

    /// OpenRouter is deliberately IN the curated-provider set even though it is
    /// an aggregator: its several-hundred-model passthrough catalog would drown
    /// any picker. Keep that trimming intact.
    #[test]
    fn openrouter_stays_trimmed_despite_being_a_passthrough() {
        let listings = vec![
            listing("openrouter", "openrouter/auto", "Auto"),
            listing(
                "openrouter",
                "meta-llama/llama-3.3-70b-instruct:free",
                "Llama",
            ),
        ];

        assert_eq!(
            curated_model_refs(&listings, &[], "", None),
            vec!["openrouter/openrouter/auto".to_string()]
        );
    }

    #[test]
    fn curates_catalog_to_short_list_and_keeps_current() {
        let listings = vec![
            listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol"),
            listing("openai", "gpt-4o", "GPT-4o"), // not curated → dropped
            listing("anthropic", "claude-opus-5", "claude-opus-5"),
            listing("gemini", "gemini-3.5-flash", "Gemini 3.5 Flash"),
        ];
        let available = vec!["GPT-5.6 Sol".to_string(), "GPT-4o".to_string()];
        let out = curated_model_names(&listings, &available, "claude-opus-5");
        // current first, then curated catalog; non-curated GPT-4o excluded.
        assert_eq!(out[0], "claude-opus-5", "current kept first");
        assert!(out.contains(&"GPT-5.6 Sol".to_string()));
        assert!(out.contains(&"Gemini 3.5 Flash".to_string()));
        assert!(!out.contains(&"GPT-4o".to_string()), "non-curated dropped");
        // current not duplicated even though it is also curated.
        assert_eq!(out.iter().filter(|m| *m == "claude-opus-5").count(), 1);
    }

    #[test]
    fn empty_listings_falls_back_to_raw_available() {
        let available = vec!["a".to_string(), "b".to_string()];
        assert_eq!(curated_model_names(&[], &available, "a"), available);
    }

    #[test]
    fn deprecated_catalog_row_is_hidden_but_current_is_retained() {
        let mut deprecated = listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol");
        deprecated.metadata.status = Some("deprecated".to_string());
        assert!(curated_model_listings(&[deprecated.clone()], "", None).is_empty());
        let current = curated_model_listings(&[deprecated], "gpt-5.6-sol", Some("openai"));
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].request_model, "gpt-5.6-sol");
    }

    #[test]
    fn provider_model_catalog_groups_in_provider_order_and_keeps_model_order() {
        let listings = vec![
            chat_listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol"),
            chat_listing("my-proxy", "llama-3.3-70b", "Llama 3.3 70B"),
            chat_listing("my-proxy", "internal-reasoner", "Internal Reasoner"),
            chat_listing("anthropic", "claude-opus-5", "Claude Opus 5"),
        ];

        let catalog = provider_model_catalog(&listings);

        assert_eq!(
            catalog
                .iter()
                .map(|entry| entry.provider_id.as_str())
                .collect::<Vec<_>>(),
            vec!["openai", "my-proxy", "anthropic"]
        );
        assert_eq!(
            catalog[1]
                .models
                .iter()
                .map(|entry| entry.request_model.as_str())
                .collect::<Vec<_>>(),
            vec!["llama-3.3-70b", "internal-reasoner"]
        );
    }

    #[test]
    fn provider_model_catalog_uses_chat_capability_and_curation_rules() {
        let mut deprecated = chat_listing("openai", "gpt-5.6-terra", "GPT-5.6 Terra");
        deprecated.metadata.status = Some("deprecated".to_string());
        let mut no_tools = chat_listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol");
        no_tools.capabilities.tools = false;
        let listings = vec![
            no_tools,
            chat_listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol"),
            chat_listing("openai", "gpt-4o", "GPT-4o"),
            deprecated,
            chat_listing("my-proxy", "internal-a", "Internal A"),
            chat_listing("my-proxy", "internal-b", "Internal B"),
        ];

        let curated = provider_model_catalog_listings(&listings);
        assert_eq!(
            curated
                .iter()
                .map(|entry| (entry.provider_id.as_str(), entry.request_model.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("openai", "gpt-5.6-sol"),
                ("my-proxy", "internal-a"),
                ("my-proxy", "internal-b"),
            ]
        );

        let catalog = provider_model_catalog(&listings);
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog[0].provider_id, "openai");
        assert_eq!(
            catalog[0]
                .models
                .iter()
                .map(|entry| entry.request_model.as_str())
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol"]
        );
        assert_eq!(catalog[1].provider_id, "my-proxy");
        assert_eq!(
            catalog[1]
                .models
                .iter()
                .map(|entry| entry.request_model.as_str())
                .collect::<Vec<_>>(),
            vec!["internal-a", "internal-b"]
        );
    }

    #[test]
    fn qualified_refs_keep_same_model_under_two_providers_distinct() {
        let listings = vec![
            listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol"),
            listing("github-copilot", "gpt-5.6-sol", "GPT-5.6 Sol"),
            listing("openai", "gpt-4o", "GPT-4o"),
        ];

        let refs = curated_model_refs(
            &listings,
            &["gpt-5.6-sol".to_string()],
            "gpt-5.6-sol",
            Some("github-copilot"),
        );

        assert_eq!(refs[0], "github-copilot/gpt-5.6-sol");
        assert!(refs.contains(&"openai/gpt-5.6-sol".to_string()));
        assert_eq!(
            refs.iter()
                .filter(|model| model.as_str() == "github-copilot/gpt-5.6-sol")
                .count(),
            1
        );
        assert!(!refs.contains(&"openai/gpt-4o".to_string()));
    }

    #[test]
    fn qualified_ref_supports_provider_local_ids_with_slashes() {
        assert_eq!(
            qualified_model_ref("openai/gpt-5.6-sol", Some("openrouter")),
            "openrouter/openai/gpt-5.6-sol"
        );
    }

    #[test]
    fn current_ref_infers_an_unambiguous_provider() {
        let listings = vec![
            listing("deepseek", "deepseek-flash", "DeepSeek V4.1 Flash"),
            listing("openai", "gpt-5.6-sol", "GPT-5.6 Sol"),
        ];

        let refs = curated_model_refs(&listings, &[], "deepseek-flash", None);

        assert_eq!(refs[0], "deepseek/deepseek-flash");
    }
}
