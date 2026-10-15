//! Runtime-owned local `/insights` producer.
//!
//! Source-bound to Claude Code 2.1.295 SHA-256
//! `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
//! Source-sensitive JavaScript work is delegated to the Bun executor;
//! Rust owns the live service calls,
//! local file writes, and the provider-neutral model route.

use crate::insights_corpus::{
    prepare_local_insights_corpus, InsightsCorpusSourceFunctions, InsightsFacetInput,
    InsightsFacetRow, InsightsSourceFuture,
};
use async_trait::async_trait;
use futures::future::join_all;
use lingxi_core::host::session_flags::scope_non_interactive_session;
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use lingxi_core::types::{ConversationMessage, MessageId};
use llm_runtime::{ApiService, MediaRoute, ProviderId};
use sidequery::{QuerySource, SideQueryClient, SideQueryRequest};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

#[cfg(all(test, feature = "desktop"))]
use crate::insights_bun_worker::BunInsightsSourceExecutor;

/// Source-preserving JSON used for Native-shaped Insights values and errors.
pub type InsightsSource = Utf16JsonProjection;
type Source = InsightsSource;

/// A Native `ft` long-session summary request plus its exact local fallback.
#[derive(Debug, Clone)]
pub struct InsightsSummaryChunk {
    /// Prompt sent to summarize this long-transcript chunk.
    pub prompt: InsightsSource,
    /// Local source excerpt used if the summary request fails.
    pub fallback: InsightsSource,
}

/// Result of source `mt` + `_t` + facet prompt construction. `direct_prompt`
/// is present for transcripts up to 30,000 JS UTF-16 units. Longer transcripts
/// provide summary chunks and require `finish_facet_prompt` after their calls.
#[derive(Debug, Clone)]
pub struct InsightsFacetQueryPlan {
    /// Complete facet prompt when the transcript fits the direct-query limit.
    pub direct_prompt: Option<InsightsSource>,
    /// Summary requests needed before building a prompt for a long transcript.
    pub summary_chunks: Vec<InsightsSummaryChunk>,
    /// Worker-owned state used to assemble the final facet prompt.
    pub opaque_plan: InsightsSource,
}

/// Ordered Native `Ot` report prompt; malformed/API-failed sections are omitted.
#[derive(Debug, Clone)]
pub struct InsightsSectionQuery {
    /// Stable name identifying the report section.
    pub name: String,
    /// Source-built prompt for this report section.
    pub prompt: InsightsSource,
    /// Maximum response tokens for this section request.
    pub max_tokens: u32,
}

/// Source-built completion fields before filesystem paths are added.
#[derive(Debug, Clone)]
pub struct InsightsRenderedReport {
    /// Pretty JSON containing the accepted report sections.
    pub insights_json: InsightsSource,
    /// Markdown header for the at-a-glance report frame.
    pub summary_header: InsightsSource,
    /// At-a-glance summary body, kept separate from its header.
    pub summary_text: InsightsSource,
    /// Rendered local HTML report body.
    pub html: InsightsSource,
}

/// Exact host facts consumed by Native `It`/`Rt`; these are snapshots of live
/// state, not values inferred from the transcript or model pricing metadata.
#[derive(Debug, Clone, Default)]
pub struct InsightsRecommendationFacts {
    /// Whether the current model and host permit an Auto-mode recommendation.
    pub auto_mode_available: bool,
    /// Whether the current session is already in Auto mode.
    pub current_session_is_auto: bool,
    /// Whether sandbox auto-allow is active for this session.
    pub sandbox_auto_allow_active: bool,
    /// Whether the permission mode currently suppresses user dialogs.
    pub current_session_sees_no_dialogs: bool,
    /// Whether the live arbiter is acting as the Auto-mode classifier.
    pub live_arbiter_is_auto_classifier: bool,
    /// Native `jte()` requires `envOnboarding === true`. The current Harness
    /// host has no such producer, so the supported local value is false.
    pub setup_skill_available: bool,
    /// Native `CR()?.environment?.length > 0` from the accepted live settings.
    pub has_auto_mode_environment: bool,
}

/// All Native string/object transforms that must retain JavaScript behavior.
/// The Desktop Bun implementation uses the byte-bound worker modules and the
/// JSON-lines worker protocol. It is not a generic Rust formatter.
pub trait InsightsSourceExecutor: InsightsCorpusSourceFunctions + Send + Sync {
    /// Build a direct facet prompt or a set of summary chunks for a session.
    fn plan_facet_query<'a>(
        &'a self,
        session_id: &'a str,
        metadata: &'a InsightsSource,
        formatted_transcript: &'a InsightsSource,
    ) -> InsightsSourceFuture<'a, InsightsFacetQueryPlan>;

    /// Combine completed chunk summaries into the final long-session facet prompt.
    fn finish_facet_prompt<'a>(
        &'a self,
        plan: &'a InsightsFacetQueryPlan,
        summaries: &'a [InsightsSource],
    ) -> InsightsSourceFuture<'a, InsightsSource>;

    /// Validate a model response and attach its session identity, if accepted.
    fn parse_facet_result<'a>(
        &'a self,
        session_id: &'a str,
        response: &'a InsightsSource,
    ) -> InsightsSourceFuture<'a, Option<InsightsSource>>;

    /// Build the ordered set of Native report-section prompts.
    fn report_section_queries<'a>(
        &'a self,
        aggregate: &'a InsightsSource,
        facets: &'a [InsightsFacetRow],
    ) -> InsightsSourceFuture<'a, Vec<InsightsSectionQuery>>;

    /// Parse one section response, returning no value when its JSON is malformed.
    fn parse_report_section<'a>(
        &'a self,
        name: &'a str,
        response: &'a InsightsSource,
    ) -> InsightsSourceFuture<'a, Option<InsightsSource>>;

    /// Build the at-a-glance prompt from aggregate, facets, and accepted sections.
    fn at_a_glance_query<'a>(
        &'a self,
        aggregate: &'a InsightsSource,
        facets: &'a [InsightsFacetRow],
        sections: &'a [(String, InsightsSource)],
    ) -> InsightsSourceFuture<'a, InsightsSource>;

    /// Parse the at-a-glance response, omitting malformed JSON.
    fn parse_at_a_glance<'a>(
        &'a self,
        response: &'a InsightsSource,
    ) -> InsightsSourceFuture<'a, Option<InsightsSource>>;

    /// Produce the optional recommendation from aggregate data and live host facts.
    fn recommendation<'a>(
        &'a self,
        aggregate: &'a InsightsSource,
        facts: &'a InsightsRecommendationFacts,
    ) -> InsightsSourceFuture<'a, Option<InsightsSource>>;

    /// Render the final JSON, summary fields, and HTML through the source worker.
    fn render_report<'a>(
        &'a self,
        aggregate: &'a InsightsSource,
        sections: &'a [(String, InsightsSource)],
        at_a_glance: Option<&'a InsightsSource>,
        recommendation: Option<&'a InsightsSource>,
    ) -> InsightsSourceFuture<'a, InsightsRenderedReport>;

    /// Return Native `yyyy-MM-dd-HHmmss` using the worker's local JS Date.
    fn report_timestamp<'a>(&'a self) -> InsightsSourceFuture<'a, String>;
}

/// Current session-selected model/profile plus the exact ApiService used for
/// every side query. The selector calls `ke()` semantics per query, rather
/// than snapshotting billing metadata or silently switching providers.
#[async_trait]
pub trait InsightsModelSelector: Send + Sync {
    /// Resolve the currently selected model and its provider profile.
    async fn selected_model(&self) -> Result<InsightsSelectedModel, InsightsSource>;
}

/// Active model route selected for an Insights side query.
#[derive(Debug, Clone)]
pub struct InsightsSelectedModel {
    /// Model identifier used for route resolution.
    pub model: String,
    /// Effective Native model source carried only when its owner and original
    /// route anchor still match this selected request route.
    pub model_source: Option<InsightsSource>,
    /// Optional profile that disambiguates the provider route.
    pub profile: Option<String>,
}

fn same_insights_request_route(left: &MediaRoute, right: &MediaRoute) -> bool {
    left.main.provider_id == right.main.provider_id
        && left.main.profile_name == right.main.profile_name
        && left.main.request_model == right.main.request_model
}

fn insights_route_uses_native_model_catalog(route: &MediaRoute) -> bool {
    matches!(
        &route.main.provider_id,
        ProviderId::AnthropicFirstParty
            | ProviderId::BedrockClaude
            | ProviderId::VertexClaude
            | ProviderId::FoundryClaude
    )
}

/// Supplies the five explicit `MZo` q4 gate results that Native folds into
/// the first memoized builtin command-name set used by `cL()`.
#[async_trait]
pub trait InsightsNativeCommandNameFactsProvider: Send + Sync {
    /// Return all five Native command-name gate facts as source JSON.
    async fn q4_open_results(&self) -> Result<InsightsSource, InsightsSource>;
}

/// Runtime producer backed by the actual session route, loaded agent-view
/// setting, process environment, and current GrowthBook-style flag snapshot.
/// Harness has no gatewayAuth credential slot, so that disjunct is absent.
pub struct RuntimeInsightsNativeCommandNameFactsProvider {
    /// Service used to resolve the active session's provider route.
    pub api: Arc<ApiService>,
    /// Session supplying the current model and profile.
    pub session: Arc<Mutex<lingxi_core::session::SessionState>>,
    /// Current setting that disables agent-view command gates.
    pub disable_agent_view_setting: bool,
}

#[async_trait]
impl InsightsNativeCommandNameFactsProvider for RuntimeInsightsNativeCommandNameFactsProvider {
    async fn q4_open_results(&self) -> Result<InsightsSource, InsightsSource> {
        let agent_view_enabled =
            lingxi_core::host::agent_view::is_enabled_with_setting(self.disable_agent_view_setting);
        let is_demo = native_boolean_environment_value("IS_DEMO");
        let skill_doctor = telemetry::feature_flags::flag_bool("tengu_lantern_prism", false)
            || native_boolean_environment_value("CLAUDE_CODE_LANTERN_PRISM");

        let (model, profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        let is_first_party = self
            .api
            .is_first_party_route(&model, profile.as_deref())
            .map_err(|error| {
                error_source(format!(
                    "insights builtin command-name route lookup failed: {error}"
                ))
            })?;

        // Native `z5()` is source-bound to false. No Harness gatewayAuth
        // credential-slot producer exists, so `ON(So())` is false here;
        // first-party is the other exact `logout` branch.
        let daemon = false;
        let logout = is_first_party;
        let fleet_fork = agent_view_enabled && !is_demo;
        let fleet_background = agent_view_enabled;
        let raw = format!(
            r#"{{"fleetFork":{fleet_fork},"fleetBackground":{fleet_background},"daemon":{daemon},"skillDoctor":{skill_doctor},"logout":{logout}}}"#
        );
        Utf16JsonProjection::parse(&raw)
            .map_err(|error| error_source(format!("insights q4 projection failed: {error}")))
    }
}

fn native_boolean_environment_value(name: &str) -> bool {
    let Ok(value) = std::env::var(name) else {
        return false;
    };
    native_boolean_value(Some(&value))
}

fn native_boolean_value(value: Option<&str>) -> bool {
    let Some(value) = value else {
        return false;
    };
    let value = lingxi_core::host::effort::trim_js_whitespace(value).to_lowercase();
    matches!(value.as_str(), "1" | "true" | "yes" | "on")
}

/// Builds the per-request Native system envelope from the exact prompt and
/// resolved provider route. The caller keeps this host context outside the
/// serialized SideQueryRequest body.
#[async_trait]
pub trait InsightsNativeSideQueryContextProvider: Send + Sync {
    /// Build per-request Native prompt context for the selected route and prompt.
    async fn context_for_request(
        &self,
        model: &str,
        profile: Option<&str>,
        first_user_text: &[u16],
    ) -> Result<Option<sidequery::NativeSideQueryPromptContext>, InsightsSource>;
}

/// Uses a service-captured Native prefix when present; Desktop supplies a
/// source-shaped prefix from its actual UA/environment snapshot when its
/// ApiService was assembled without one (the normal Desktop composition).
pub struct RuntimeInsightsNativeSideQueryContextProvider {
    /// Service holding the provider route and any captured system prefix.
    pub api: Arc<ApiService>,
    /// Desktop prefix used when the service has no captured prefix.
    pub fallback_prefix:
        Option<lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix>,
}

#[async_trait]
impl InsightsNativeSideQueryContextProvider for RuntimeInsightsNativeSideQueryContextProvider {
    async fn context_for_request(
        &self,
        model: &str,
        profile: Option<&str>,
        first_user_text: &[u16],
    ) -> Result<Option<sidequery::NativeSideQueryPromptContext>, InsightsSource> {
        let route = self
            .api
            .resolve_media_route(model, profile)
            .map_err(|error| {
                error_source(format!("insights system-prefix route failed: {error}"))
            })?;
        let protocol = self
            .api
            .protocol_for_model(model, profile)
            .map_err(|error| {
                error_source(format!("insights system-prefix protocol failed: {error}"))
            })?;
        let identity = match (&route.main.provider_id, protocol) {
            (ProviderId::AnthropicFirstParty, llm_runtime::ProtocolFamily::AnthropicMessages)
            | (ProviderId::BedrockClaude, llm_runtime::ProtocolFamily::BedrockClaude)
            | (ProviderId::FoundryClaude, llm_runtime::ProtocolFamily::FoundryClaude) => {
                branding::NATIVE_AGENT_SDK_IDENTITY
            }
            (ProviderId::VertexClaude, llm_runtime::ProtocolFamily::VertexClaude) => {
                orchestrator::prompt::locked_templates::HEADER
            }
            _ => return Ok(None),
        };
        let Some(mut prefix) = self
            .api
            .native_system_prefix_snapshot()
            .or_else(|| self.fallback_prefix.clone())
        else {
            return Ok(None);
        };
        use lingxi_llm_client::providers::anthropic::system_prompt::{
            NativePromptAttribution, PromptText,
        };
        // Native bNn selects the Vertex CLI identity for Vertex Claude and the
        // Agent SDK role for every other supported Claude Messages provider.
        // Brand mapping is local to this cloned Insights request; preserve the
        // captured billing, attribution, version, and route assumptions.
        prefix.host_identity = PromptText::from_string(identity);
        Ok(Some(sidequery::NativeSideQueryPromptContext {
            prefix,
            attribution: NativePromptAttribution {
                first_user_text: PromptText::from_utf16(first_user_text.to_vec()),
                is_subagent: false,
                workload: None,
                previous_request_id: None,
                prompt_id: None,
                turn_origin: None,
                turn_position: None,
            },
            include_host_identity: true,
        }))
    }
}

/// Construct the source-pinned Desktop fallback from the current host's actual
/// UA and environment snapshot. Headless can reuse the prefix already captured
/// by ApiService; Desktop/Bridge currently do not install that main-request
/// prefix, so Insights supplies it only on its own first-party SideQuery.
#[cfg(feature = "desktop")]
pub(crate) fn desktop_native_insights_system_prefix(
    config: &crate::desktop::DesktopConfig,
    user_agent: &llm_runtime::model::user_agent::UserAgentEnv,
) -> lingxi_llm_client::providers::anthropic::system_prompt::NativeSystemPrefix {
    use lingxi_llm_client::providers::anthropic::system_prompt::{NativeSystemPrefix, PromptText};

    let environment_value = |name: &str| match config.oauth_environment_lookup.as_ref() {
        Some(lookup) => lookup.get(name),
        None => std::env::var(name).ok(),
    };
    let attribution_enabled =
        !environment_value("CLAUDE_CODE_ATTRIBUTION_HEADER").is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        });
    NativeSystemPrefix {
        compatibility_version: crate::headless::CLAUDE_CODE_REFERENCE_VERSION.into(),
        entrypoint: user_agent
            .entrypoint
            .clone()
            .unwrap_or_else(|| "cli".into()),
        host_identity: PromptText::from_string(branding::NATIVE_AGENT_SDK_IDENTITY),
        attribution_enabled,
        assume_first_party_base_url: environment_value("_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL")
            .is_some_and(|value| !value.is_empty()),
    }
}

/// Main-loop model selector using the same session route and model resolver as
/// the normal request path. Composition supplies the current session and the
/// `RuntimeModelResolutionProvider` created from the same `ModelRuntime`.
pub struct RuntimeInsightsModelSelector {
    /// Live session model/profile selected for the next request.
    pub session: Arc<Mutex<lingxi_core::session::SessionState>>,
    /// Service that resolves the selected model's provider route.
    pub api: Arc<ApiService>,
    /// Same-runtime model resolver used for the source-selected Opus family.
    pub model_resolution: Arc<dyn agent::model_resolution::ModelResolutionContextProvider>,
    /// Live configured source binding lock owned by the current orchestrator.
    /// Explicit session picks and per-query Native model selections do not own it.
    pub main_model_source: Arc<std::sync::RwLock<Option<lingxi_core::host::ModelSourceBinding>>>,
}

#[async_trait]
impl InsightsModelSelector for RuntimeInsightsModelSelector {
    async fn selected_model(&self) -> Result<InsightsSelectedModel, InsightsSource> {
        let (current_model, current_profile, source_owner, user_model_setting) = {
            let session = self.session.lock().await;
            (
                session.model.clone(),
                session.model_profile.clone(),
                session.user_model_setting_source,
                session.user_model_setting.clone(),
            )
        };
        let route = self
            .api
            .resolve_media_route(&current_model, current_profile.as_deref())
            .map_err(|error| {
                error_source(format!("insights current model route failed: {error}"))
            })?;
        let first_party = matches!(&route.main.provider_id, ProviderId::AnthropicFirstParty);
        let use_opus_family = if first_party {
            self.first_party_opus_branch(&current_model, current_profile.as_deref())
                .await?
        } else {
            // Native `mUn()` is false on the Harness Bedrock, Vertex, Foundry,
            // and non-Anthropic provider identities. Native direct AWS/GCP
            // identities are not represented by ProviderId and stay outside
            // this source-mapped branch.
            false
        };

        if !use_opus_family {
            let model_source = self
                .model_source_for_request(
                    &current_model,
                    current_profile.as_deref(),
                    &route,
                    source_owner,
                    user_model_setting.as_ref(),
                )
                .await;
            return Ok(InsightsSelectedModel {
                model: current_model,
                model_source,
                profile: current_profile,
            });
        }

        let parent = self
            .model_resolution
            .context_for_route(&current_model, current_profile.as_deref())
            .map_err(|error| error_source(format!("insights Opus parent route failed: {error}")))?;
        let selected = agent::model_resolution::resolve_user_model_selection(
            "opus",
            current_profile.as_deref(),
            &parent,
            self.model_resolution.as_ref(),
        )
        .map_err(|error| error_source(format!("insights Opus model resolution failed: {error}")))?;
        Ok(InsightsSelectedModel {
            model: selected.model,
            // Native `ke()` selected a per-query Opus route. The configured
            // parent source belongs to the session route and must not override it.
            model_source: None,
            profile: selected.model_profile,
        })
    }
}

impl RuntimeInsightsModelSelector {
    async fn model_source_for_request(
        &self,
        session_model: &str,
        session_profile: Option<&str>,
        request_route: &MediaRoute,
        source_owner: lingxi_core::session::SessionModelSettingSource,
        user_model_setting: Option<&lingxi_core::host::ModelSourceBinding>,
    ) -> Option<InsightsSource> {
        if !insights_route_uses_native_model_catalog(request_route) {
            return None;
        }
        {
            let session = self.session.lock().await;
            if session.model != session_model
                || session.model_profile.as_deref() != session_profile
                || session.user_model_setting_source != source_owner
                || (source_owner == lingxi_core::session::SessionModelSettingSource::UserSelection
                    && session.user_model_setting.as_ref() != user_model_setting)
            {
                return None;
            }
        }
        let binding = match source_owner {
            lingxi_core::session::SessionModelSettingSource::Configured => self
                .main_model_source
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()?,
            lingxi_core::session::SessionModelSettingSource::UserSelection => {
                user_model_setting?.clone()
            }
        };
        let anchor_route = self
            .api
            .resolve_media_route(
                &binding.route_anchor.model,
                binding.route_anchor.profile.as_deref(),
            )
            .ok()?;
        if !same_insights_request_route(&anchor_route, request_route) {
            return None;
        }
        binding.effective_source_for_route(
            &binding.route_anchor.model,
            binding.route_anchor.profile.as_deref(),
        )
    }

    async fn first_party_opus_branch(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<bool, Source> {
        let facts = self
            .api
            .classifier_model_route_facts(model, profile)
            .await
            .map_err(|error| {
                error_source(format!("insights first-party route facts failed: {error}"))
            })?;
        if facts.provider.as_deref() != Some("firstParty") {
            return Ok(false);
        }
        if facts.native_base_url {
            return Ok(true);
        }
        if !facts.native_api_key {
            return Ok(false);
        }
        // Native `Bvs()` additionally excludes the `/login managed key`
        // credential source. Harness currently has no enum variant for that
        // identity. Only the direct API-key-helper and named environment
        // source are proven safe mappings for a custom first-party endpoint.
        let source = self
            .api
            .credential_source(model, profile)
            .await
            .map_err(|error| error_source(format!("insights credential source failed: {error}")))?;
        match source {
            llm_runtime::CredentialSource::ApiKeyHelper => Ok(true),
            llm_runtime::CredentialSource::Environment { variable }
                if variable == "ANTHROPIC_API_KEY" =>
            {
                Ok(true)
            }
            llm_runtime::CredentialSource::Stored
            | llm_runtime::CredentialSource::Configured
            | llm_runtime::CredentialSource::Unknown => Err(error_source(
                "insights cannot classify a custom first-party managed-key source",
            )),
            llm_runtime::CredentialSource::None
            | llm_runtime::CredentialSource::Environment { .. }
            | llm_runtime::CredentialSource::OAuth => Ok(false),
        }
    }
}

fn native_provider_name(route: &MediaRoute) -> Option<&'static str> {
    match &route.main.provider_id {
        ProviderId::AnthropicFirstParty => Some("firstParty"),
        ProviderId::BedrockClaude => Some("bedrock"),
        ProviderId::VertexClaude => Some("vertex"),
        ProviderId::FoundryClaude => Some("foundry"),
        _ => None,
    }
}

/// Reads the live policy and shell/sandbox facts used by Native `Rt`. The
/// local host has no `envOnboarding` setting producer, so setup-skill
/// availability is source-backed false even when a similarly named command
/// happens to be registered.
pub struct RuntimeInsightsRecommendationProvider {
    /// Live gate supplying permission policy and current permission mode.
    pub permission_gate: Arc<dyn lingxi_core::host::permission_gate::PermissionGate>,
    /// Current sandbox runtime facts used by the recommendation predicate.
    pub tool_context: Arc<tool_api::BuiltinToolContext>,
    /// Service used to resolve the current provider and model facts.
    pub api: Arc<ApiService>,
    /// Current session model/profile used for route resolution.
    pub session: Arc<Mutex<lingxi_core::session::SessionState>>,
}

impl RuntimeInsightsRecommendationProvider {
    /// Snapshot live permission, model-route, and sandbox facts for Native recommendations.
    pub async fn snapshot(&self) -> InsightsRecommendationFacts {
        let current_mode = self.permission_gate.permission_mode().unwrap_or_default();
        let sandbox = self.tool_context.effective_sandbox_runtime();
        let sandbox_auto_allow_active = self.tool_context.sandbox_available
            && sandbox.enabled
            && sandbox.auto_allow_bash_if_sandboxed;
        let Some(policy) = self.permission_gate.permission_policy_snapshot() else {
            // Native optional-chains absent permissionGates into false/no
            // auto-policy recommendation. The current permission mode and
            // sandbox runtime are separate live host facts and remain visible.
            return InsightsRecommendationFacts {
                current_session_is_auto: current_mode == "auto",
                sandbox_auto_allow_active,
                current_session_sees_no_dialogs: matches!(
                    current_mode.as_str(),
                    "bypassPermissions" | "dontAsk"
                ),
                live_arbiter_is_auto_classifier: current_mode == "auto",
                ..InsightsRecommendationFacts::default()
            };
        };
        let (current_model, current_profile) = {
            let session = self.session.lock().await;
            (session.model.clone(), session.model_profile.clone())
        };
        let Ok(route) = self
            .api
            .resolve_media_route(&current_model, current_profile.as_deref())
        else {
            // Recommendation facts are optional. A report still succeeds when
            // the current route cannot provide Native's model/provider facts.
            return InsightsRecommendationFacts::default();
        };
        let provider = native_provider_name(&route).unwrap_or("other");
        let auto_mode_available = permission::auto_mode_available(&permission::AutoGateInputs {
            disabled_by_settings: policy.auto_mode_disabled,
            circuit_broken: policy.native_auto_mode_circuit_broken || policy.denial_circuit_broken,
            model: route.main.request_model.clone(),
            provider: provider.to_owned(),
        });
        let mode = policy.permission_mode.as_str();
        let has_auto_mode_environment = policy
            .classifier_auto_mode_settings
            .as_ref()
            .is_some_and(|settings| !settings.environment.is_empty());
        InsightsRecommendationFacts {
            auto_mode_available,
            current_session_is_auto: mode == "auto",
            sandbox_auto_allow_active,
            current_session_sees_no_dialogs: matches!(mode, "bypassPermissions" | "dontAsk"),
            // Native `gi(mode)` is true in Auto, or in Plan while the live Auto
            // arbiter is active. PolicyPermissionGate snapshots that latch.
            live_arbiter_is_auto_classifier: mode == "auto"
                || (mode == "plan" && policy.active_auto_mode),
            setup_skill_available: false,
            has_auto_mode_environment,
        }
    }
}

/// Live Desktop implementation. The provider and source executor both retain
/// the same ApiService; no raw API key or alternate HTTP client is introduced.
pub struct LocalInsightsReportProvider {
    config_root: PathBuf,
    command_availability: command_api::builtins::InsightsCommandAvailability,
    side_query: InsightsSideQueryExecutor,
    recommendation: Arc<RuntimeInsightsRecommendationProvider>,
    source: Arc<dyn InsightsSourceExecutor>,
    command_name_facts: Arc<dyn InsightsNativeCommandNameFactsProvider>,
}

/// Minimal source-call seam used by both the report provider and captured
/// SideQuery integration tests. It deliberately owns only model selection and
/// the shared SideQueryClient, so tests exercise the same request construction
/// without assembling corpus, report, or permission-gate state.
#[derive(Clone)]
pub struct InsightsSideQueryExecutor {
    side_queries: Arc<dyn SideQueryClient>,
    model_selector: Arc<dyn InsightsModelSelector>,
    native_prompt_context: Arc<dyn InsightsNativeSideQueryContextProvider>,
}

impl InsightsSideQueryExecutor {
    /// Bind a shared query client to current model and Native prompt-context providers.
    pub fn new(
        side_queries: Arc<dyn SideQueryClient>,
        model_selector: Arc<dyn InsightsModelSelector>,
        native_prompt_context: Arc<dyn InsightsNativeSideQueryContextProvider>,
    ) -> Self {
        Self {
            side_queries,
            model_selector,
            native_prompt_context,
        }
    }

    /// Send one non-streaming Insights query and return its text response.
    pub async fn query(
        &self,
        prompt: Utf16JsonProjection,
        max_tokens: u32,
    ) -> Result<Utf16JsonProjection, Utf16JsonProjection> {
        // Native `isNonInteractiveSession: true` is task-local around both
        // request creation and service dispatch, not a provider JSON field.
        scope_non_interactive_session(true, async {
            let selected = self.model_selector.selected_model().await?;
            let units = prompt
                .string_units("")
                .ok_or_else(|| error_source("insights worker query prompt is not a string"))?;
            let native_prompt_context = self
                .native_prompt_context
                .context_for_request(&selected.model, selected.profile.as_deref(), &units)
                .await?;
            let text = String::from_utf16_lossy(&units);
            let request = SideQueryRequest {
                // Native bze defaults enablePromptCaching to false.
                enable_prompt_caching: false,
                model_attempt: None,
                model: selected.model,
                model_source: selected.model_source,
                profile: selected.profile,
                system_prompt: None,
                typed_attachment_ids: Default::default(),
                task_deliveries: Default::default(),
                system_prompt_input: None,
                messages: vec![ConversationMessage::user_meta_js_utf16(
                    MessageId::new(),
                    text,
                    units,
                )],
                tools: Vec::new(),
                tool_choice: None,
                output_format: None,
                max_tokens: Some(max_tokens),
                // ProviderSideQueryClient leaves caller retry budgets to the
                // ApiService. Native has no per-call maxRetries override.
                max_retries: 0,
                temperature: None,
                thinking: Some(llm_runtime::model::thinking::ThinkingConfig::Disabled),
                effort: None,
                stop_sequences: Vec::new(),
                query_source: QuerySource::Custom("insights".into()),
                native_prompt_context,
                // Native supplies an empty caller system prompt here, but the
                // SDK still owns its per-query Anthropic prefix and billing
                // attribution. hasAppendSystemPrompt=false does not remove
                // those SDK-generated blocks.
                skip_system_prompt_prefix: false,
            };
            self.side_queries
                .query(request)
                .await
                .map_err(|error| error_source(format!("insights side query failed: {error}")))?
                .text
                .ok_or_else(|| error_source("insights side query returned no text"))
        })
        .await
    }
}

impl LocalInsightsReportProvider {
    /// Construct the local report producer from host facts and shared runtime services.
    pub fn new(
        config_root: PathBuf,
        command_availability: command_api::builtins::InsightsCommandAvailability,
        side_queries: Arc<dyn SideQueryClient>,
        model_selector: Arc<dyn InsightsModelSelector>,
        native_prompt_context: Arc<dyn InsightsNativeSideQueryContextProvider>,
        recommendation: Arc<RuntimeInsightsRecommendationProvider>,
        source: Arc<dyn InsightsSourceExecutor>,
        command_name_facts: Arc<dyn InsightsNativeCommandNameFactsProvider>,
    ) -> Self {
        Self {
            config_root,
            command_availability,
            side_query: InsightsSideQueryExecutor::new(
                side_queries,
                model_selector,
                native_prompt_context,
            ),
            recommendation,
            source,
            command_name_facts,
        }
    }

    async fn run_facet_query(&self, input: InsightsFacetInput) -> Option<(String, Source)> {
        let plan = self
            .source
            .plan_facet_query(
                &input.session_id,
                &input.metadata,
                &input.formatted_transcript,
            )
            .await
            .ok()?;
        let prompt = if let Some(prompt) = plan.direct_prompt.as_ref() {
            prompt.clone()
        } else {
            let summaries = join_all(plan.summary_chunks.iter().map(|chunk| async move {
                match self.side_query.query(chunk.prompt.clone(), 500).await {
                    Ok(response) => response,
                    Err(_) => chunk.fallback.clone(),
                }
            }))
            .await;
            self.source
                .finish_facet_prompt(&plan, &summaries)
                .await
                .ok()?
        };
        let response = self.side_query.query(prompt, 4096).await.ok()?;
        let facet = self
            .source
            .parse_facet_result(&input.session_id, &response)
            .await
            .ok()??;
        Some((input.session_id, facet))
    }
}

#[async_trait]
impl command_api::builtins::insights::InsightsReportProvider for LocalInsightsReportProvider {
    fn command_availability(&self) -> command_api::builtins::InsightsCommandAvailability {
        self.command_availability
    }

    async fn generate_report(
        &self,
    ) -> Result<command_api::builtins::insights::InsightsReport, InsightsSource> {
        let q4_open_results = self.command_name_facts.q4_open_results().await?;
        let mut corpus =
            prepare_local_insights_corpus(&self.config_root, self.source.clone(), q4_open_results)
                .await?;
        // Native Wt reads up to fifty uncached facet sessions and each facet's
        // long-session summaries concurrently, then preserves result order.
        let facet_results = join_all(
            corpus
                .facet_work
                .drain(..)
                .map(|input| self.run_facet_query(input)),
        )
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let corpus = corpus.finish(facet_results).await?;

        let query_plan = self
            .source
            .report_section_queries(&corpus.aggregate, &corpus.facets)
            .await?;
        let section_results = join_all(query_plan.into_iter().map(|query| async move {
            let response = self
                .side_query
                .query(query.prompt, query.max_tokens)
                .await
                .ok()?;
            let parsed = self
                .source
                .parse_report_section(&query.name, &response)
                .await
                .ok()??;
            Some((query.name, parsed))
        }))
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

        let at_a_glance_prompt = self
            .source
            .at_a_glance_query(&corpus.aggregate, &corpus.facets, &section_results)
            .await?;
        let at_a_glance = match self.side_query.query(at_a_glance_prompt, 8192).await {
            Ok(response) => self.source.parse_at_a_glance(&response).await?,
            Err(_) => None,
        };
        let recommendation_facts = self.recommendation.snapshot().await;
        let recommendation_line = self
            .source
            .recommendation(&corpus.aggregate, &recommendation_facts)
            .await?;
        let rendered = self
            .source
            .render_report(
                &corpus.aggregate,
                &section_results,
                at_a_glance.as_ref(),
                recommendation_line.as_ref(),
            )
            .await?;

        let stamp = self.source.report_timestamp().await?;
        let report_root = self.config_root.join("usage-data");
        let timestamped = report_root.join(format!("report-{stamp}.html"));
        // Native `ot` swallows only recursive mkdir failure, then attempts the
        // timestamped file followed by the stable report.html file.
        let _ = tokio::fs::create_dir_all(&report_root).await;
        write_report_file(&timestamped, &rendered.html).await?;
        write_report_file(&report_root.join("report.html"), &rendered.html).await?;

        let html_path = path_string(&timestamped);
        let report_url = format!("file://{html_path}");
        let facets_directory = path_string(&report_root.join("facets"));
        Ok(command_api::builtins::insights::InsightsReport {
            insights_json: rendered.insights_json,
            report_url: string_source(&report_url),
            html_path: string_source(&html_path),
            facets_directory: string_source(&facets_directory),
            summary_header: rendered.summary_header,
            summary_text: rendered.summary_text,
            auto_mode_recommendation: recommendation_line,
        })
    }
}

async fn write_report_file(path: &Path, source: &Source) -> Result<(), Source> {
    let units = source
        .string_units("")
        .ok_or_else(|| error_source("insights HTML renderer returned a non-string"))?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .await
        .map_err(|error| error_source(format!("insights report write failed: {error}")))?;
    use tokio::io::AsyncWriteExt;
    file.write_all(String::from_utf16_lossy(&units).as_bytes())
        .await
        .map_err(|error| error_source(format!("insights report write failed: {error}")))?;
    file.flush()
        .await
        .map_err(|error| error_source(format!("insights report write failed: {error}")))
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn string_source(value: &str) -> Source {
    let units = value.encode_utf16().collect::<Vec<_>>();
    Source::root_string(value.to_owned(), units).expect("Rust strings are valid UTF-16")
}

pub(crate) fn error_source(message: impl AsRef<str>) -> Source {
    string_source(message.as_ref())
}

/// Helpers used by the worker RPC encoder; they import every child projection
/// without passing through serde_json::Value or UTF-8 string conversion.
pub(crate) fn object_source(
    fields: impl IntoIterator<Item = (String, Source)>,
) -> Result<Source, Source> {
    let mut builder = lingxi_core::types::utf16_json::Utf16JsonBuilder::new();
    let mut nodes = Vec::new();
    for (key, value) in fields {
        let id = builder
            .import_projection(&value)
            .map_err(|error| error_source(format!("insights worker payload error: {error}")))?;
        nodes.push((key, id));
    }
    let root = builder
        .push_object(nodes)
        .map_err(|error| error_source(format!("insights worker payload error: {error}")))?;
    builder
        .finish(root)
        .map_err(|error| error_source(format!("insights worker payload error: {error}")))
}

pub(crate) fn array_source(values: &[Source]) -> Result<Source, Source> {
    let mut builder = lingxi_core::types::utf16_json::Utf16JsonBuilder::new();
    let mut nodes = Vec::with_capacity(values.len());
    for value in values {
        nodes.push(
            builder
                .import_projection(value)
                .map_err(|error| error_source(format!("insights worker payload error: {error}")))?,
        );
    }
    let root = builder
        .push_array(nodes)
        .map_err(|error| error_source(format!("insights worker payload error: {error}")))?;
    builder
        .finish(root)
        .map_err(|error| error_source(format!("insights worker payload error: {error}")))
}

pub(crate) fn source_string(source: &Source) -> Option<String> {
    source
        .string_units("")
        .map(|units| String::from_utf16_lossy(&units))
}

pub(crate) fn source_field(source: &Source, pointer: &str) -> Option<Source> {
    source.subprojection(pointer).ok()
}

pub(crate) fn source_is_string(source: &Source) -> bool {
    source
        .at_pointer("")
        .is_some_and(|value| value.code_units().is_some())
}

#[cfg(test)]
#[path = "insights_query_tests.rs"]
mod insights_query_tests;

#[cfg(all(test, feature = "desktop"))]
#[path = "insights_report_tests.rs"]
mod insights_report_tests;

#[cfg(test)]
mod native_command_gate_tests {
    use super::native_boolean_value;

    #[test]
    fn native_boolean_environment_parser_matches_trimmed_le_tokens() {
        for value in ["1", " TRUE ", "yes", "On", "\u{feff}true\u{feff}"] {
            assert!(native_boolean_value(Some(value)), "{value:?}");
        }
        for value in ["", "false", "off", "0", " true no"] {
            assert!(!native_boolean_value(Some(value)), "{value:?}");
        }
        assert!(!native_boolean_value(None));
    }
}
