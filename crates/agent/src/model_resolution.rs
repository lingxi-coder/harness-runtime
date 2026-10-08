//! Resolve an agent definition's model preference to a concrete wire model id.
//!
//! Provider identity and alias defaults come from the host's selected route.
//! Model changes resolve a model and profile together, so overlapping catalogs
//! retain the intended endpoint and credentials. The managed allowlist and
//! permission-mode policy remain part of subagent selection.

use crate::definition::{AgentDefinition, AgentModel, AgentSource};
use llm_runtime::model::allowlist::{self, ModelEnforcement};
use permission::PermissionMode;

/// Provider kind that affects Native alias and legacy-model decisions.
///
/// This is intentionally not inferred from a profile name or endpoint. A host
/// must map it from the configured provider identity; unknown/custom providers
/// stay `Other` until they have an explicit identity source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelProviderKind {
    /// Anthropic's first-party provider.
    FirstParty,
    /// Claude on Amazon Bedrock (`bedrock`, distinct from `anthropicAws`).
    Bedrock,
    /// Claude on Vertex.
    Vertex,
    /// Claude on Azure AI Foundry.
    Foundry,
    /// Explicit Anthropic AWS service provider identity.
    AnthropicAws,
    /// Explicit Anthropic Google Cloud provider identity.
    AnthropicGoogleCloud,
    /// Anthropic Mantle provider identity.
    Mantle,
    /// Explicit Anthropic gateway provider identity.
    Gateway,
    /// Other or not-yet-classified provider.
    #[default]
    Other,
}

/// Exact selected route facts used by model resolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelRouteFacts {
    /// Current concrete/requested model id.
    pub model: String,
    /// Selected profile, if the host has an explicit profile for this route.
    pub profile: Option<String>,
    /// Provider identity from the route resolver/configuration.
    pub provider: Option<ModelProviderKind>,
    /// Configured endpoint. This is descriptive route data; it is not used to
    /// guess provider kind.
    pub endpoint: Option<String>,
    /// Configured protocol family, when known.
    pub protocol: Option<String>,
}

/// Family aliases selected from the active route's host catalog or explicit
/// family overrides. `Some("")` is a real configured override and must remain
/// distinct from `None`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FamilyModelDefaults {
    /// `opus` family default or explicit override.
    pub opus: Option<String>,
    /// `sonnet` family default or explicit override.
    pub sonnet: Option<String>,
    /// `haiku` family default or explicit override.
    pub haiku: Option<String>,
    /// `fable` family default or explicit override.
    pub fable: Option<String>,
}

impl FamilyModelDefaults {
    /// Return one configured family default without collapsing `Some("")`.
    #[must_use]
    pub fn get(&self, family: &str) -> Option<&str> {
        match family {
            "opus" => self.opus.as_deref(),
            "sonnet" => self.sonnet.as_deref(),
            "haiku" => self.haiku.as_deref(),
            "fable" => self.fable.as_deref(),
            _ => None,
        }
    }
}

/// Presence-sensitive, host-owned context for one selected model route.
///
/// `best_model`, `fable_strategy_available`, `native_1m`, and the entitlement
/// snapshot are optional because the current runtime does not yet provide the
/// corresponding Native catalog/feature sources. Missing values are not
/// synthesized from profile names or endpoint URLs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelResolutionContext {
    /// Resolved or explicitly selected route.
    pub route: ModelRouteFacts,
    /// Defaults/overrides for the active route.
    pub family_defaults: FamilyModelDefaults,
    /// Concrete provider-local IDs registered on this route. These are host
    /// catalog facts, so a wire ID named `opus` is distinct from an alias.
    pub registered_model_ids: std::collections::BTreeSet<String>,
    /// Explicit catalog aliases on this route, with normalized keys and
    /// distinct wire-model candidates. Multiple candidates remain ambiguous.
    pub catalog_aliases: std::collections::BTreeMap<String, Vec<String>>,
    /// Native's dynamically selected `best` model, when a trusted source exists.
    pub best_model: Option<String>,
    /// Whether the Native Fable strategy is available for this host/session.
    pub fable_strategy_available: Option<bool>,
    /// Native 1M support for the model currently being resolved, when known.
    pub native_1m: Option<bool>,
    /// Explicit entitlement result, when a trusted source exists.
    pub entitlement_allowed: Option<bool>,
    /// LingXi switch equivalent to Native's truthy 1M-disable gate.
    pub disable_1m_context: bool,
}

/// Host route lookup failure. The Agent layer reports ambiguity and missing
/// configured routes instead of selecting the first/last same-named profile.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ModelResolutionError {
    /// No configured route could serve the requested model/profile.
    #[error("no configured model route for model {model:?} and profile {profile:?}: {reason}")]
    RouteUnavailable {
        /// Requested model.
        model: String,
        /// Requested profile, if supplied.
        profile: Option<String>,
        /// Route resolver detail.
        reason: String,
    },
    /// More than one configured profile can serve an unqualified model.
    #[error("model {model:?} is served by multiple profiles: {profiles:?}")]
    AmbiguousRoute {
        /// Requested model.
        model: String,
        /// Matching profile names.
        profiles: Vec<String>,
    },
    /// The host did not provide an actual default for this family on the route.
    #[error("no {family} model default is available for profile {profile:?}")]
    MissingFamilyDefault {
        /// Missing family name.
        family: String,
        /// Selected profile, if available.
        profile: Option<String>,
    },
}

/// Synchronous route-context provider. Implementations may own a runtime or
/// config snapshot internally, but callers exchange only owned route facts.
pub trait ModelResolutionContextProvider: Send + Sync {
    /// Resolve route facts for a concrete model/profile pair.
    fn context_for_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<ModelResolutionContext, ModelResolutionError>;
}

/// A model and its provider route resolved together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelSelection {
    /// Provider-local model sent on the wire.
    pub model: String,
    /// Profile that selects the configured endpoint and credentials.
    pub model_profile: Option<String>,
    /// Facts for the selected route, including its scoped aliases.
    pub model_resolution_context: ModelResolutionContext,
}

/// Resolve a user model preference while keeping model and provider identity
/// together. Relative family aliases use the parent's catalog. Concrete models
/// prefer that route, and may select another configured route when unavailable
/// there. A qualified `profile/model` reference is resolved by the host registry.
pub fn resolve_user_model_selection(
    model_input: &str,
    model_profile: Option<&str>,
    parent_context: &ModelResolutionContext,
    provider: &dyn ModelResolutionContextProvider,
) -> Result<ResolvedModelSelection, ModelResolutionError> {
    let model_input = lingxi_core::host::effort::trim_js_whitespace(model_input);
    let relative_alias = is_relative_model_alias(model_input);
    let (context, resolve_alias) = if let Some(profile) = model_profile {
        let context = provider.context_for_route(model_input, Some(profile))?;
        let resolve_alias = relative_alias && !is_registered_concrete_model(model_input, &context);
        (context, resolve_alias)
    } else if relative_alias {
        // A retired parent model is not needed to retrieve its profile's
        // aliases. Only a registered concrete ID may discover another route
        // when this profile has no meaning for the relative preference.
        let scoped = parent_context
            .route
            .profile
            .as_deref()
            .and_then(|profile| provider.context_for_route(model_input, Some(profile)).ok())
            .filter(|context| {
                is_registered_concrete_model(model_input, context)
                    || has_relative_model_preference(model_input, context)
            })
            .unwrap_or_else(|| parent_context.clone());
        if is_registered_concrete_model(model_input, &scoped) {
            let context =
                provider.context_for_route(model_input, scoped.route.profile.as_deref())?;
            (context, false)
        } else if has_relative_model_preference(model_input, &scoped) {
            (scoped, true)
        } else {
            match provider.context_for_route(model_input, None) {
                Ok(context) if is_registered_concrete_model(model_input, &context) => {
                    (context, false)
                }
                Ok(context)
                    if parent_context.route.profile.is_none()
                        && has_relative_model_preference(model_input, &context) =>
                {
                    (context, true)
                }
                Err(error @ ModelResolutionError::AmbiguousRoute { .. }) => return Err(error),
                _ => (scoped, true),
            }
        }
    } else {
        let context = match provider
            .context_for_route(model_input, parent_context.route.profile.as_deref())
        {
            Ok(context) => context,
            Err(ModelResolutionError::RouteUnavailable { .. })
                if parent_context.route.profile.is_some() =>
            {
                provider.context_for_route(model_input, None)?
            }
            Err(error) => return Err(error),
        };
        (context, false)
    };
    let model = if resolve_alias {
        resolve_user_specified_model(model_input, &context)?
    } else {
        // The host already resolved the request to a concrete wire ID. Never
        // reinterpret that ID as a native family or strategy name.
        let model = &context.route.model;
        if !context.disable_1m_context && has_1m_context(model) {
            normalize_1m_suffix(model)
        } else {
            model.clone()
        }
    };
    let final_context = provider.context_for_route(&model, context.route.profile.as_deref())?;
    Ok(ResolvedModelSelection {
        model: final_context.route.model.clone(),
        model_profile: final_context.route.profile.clone(),
        model_resolution_context: final_context,
    })
}

pub(crate) fn is_registered_concrete_model(model: &str, context: &ModelResolutionContext) -> bool {
    let model = lingxi_core::host::effort::trim_js_whitespace(model);
    let bare = strip_1m_suffix(model);
    context.registered_model_ids.iter().any(|registered| {
        registered.eq_ignore_ascii_case(model) || registered.eq_ignore_ascii_case(&bare)
    })
}

pub(crate) fn has_relative_model_preference(model: &str, context: &ModelResolutionContext) -> bool {
    let base = strip_1m_suffix(&model.to_lowercase());
    context.catalog_aliases.contains_key(&base)
        || match base.as_str() {
            "opus" | "sonnet" | "haiku" | "fable" => context.family_defaults.get(&base).is_some(),
            "opusplan" => context.family_defaults.sonnet.is_some(),
            "best" => context.best_model.is_some() || context.family_defaults.opus.is_some(),
            _ => false,
        }
}

/// Resolve a skill model preference using the live parent route. An inherited
/// context-window suffix is retained only when the host confirms that the
/// target on that same route supports it. Explicit suffixes retain the normal
/// user-model semantics.
pub fn resolve_skill_model_selection(
    model_input: &str,
    model_profile: Option<&str>,
    parent_context: &ModelResolutionContext,
    provider: &dyn ModelResolutionContextProvider,
) -> Result<ResolvedModelSelection, ModelResolutionError> {
    let selection =
        resolve_user_model_selection(model_input, model_profile, parent_context, provider)?;
    let target = &selection.model_resolution_context;
    if !has_1m_context(&parent_context.route.model)
        || has_1m_context(model_input)
        || has_1m_context(&selection.model)
        || parent_context.disable_1m_context
        || target.disable_1m_context
        || target.native_1m != Some(true)
        || target.route.provider.is_none()
        || target.route.profile != parent_context.route.profile
        || target.route.provider != parent_context.route.provider
        || target.route.endpoint != parent_context.route.endpoint
        || target.route.protocol != parent_context.route.protocol
        || (target.route.provider == Some(ModelProviderKind::Other)
            && target.route.profile.is_none()
            && target.route.endpoint.is_none())
    {
        return Ok(selection);
    }
    resolve_user_model_selection(
        &format!("{}[1m]", selection.model),
        selection.model_profile.as_deref(),
        parent_context,
        provider,
    )
}

/// Whether a model preference names a route-scoped family or reserved model
/// strategy, optionally with a context-window suffix.
#[must_use]
pub fn is_relative_model_alias(model: &str) -> bool {
    let normalized = lingxi_core::host::effort::trim_js_whitespace(model).to_lowercase();
    let base = strip_1m_suffix(&normalized);
    matches!(
        base.as_str(),
        "opus" | "sonnet" | "haiku" | "fable" | "best" | "opusplan"
    )
}

impl<F> ModelResolutionContextProvider for F
where
    F: Fn(&str, Option<&str>) -> Result<ModelResolutionContext, ModelResolutionError> + Send + Sync,
{
    fn context_for_route(
        &self,
        model: &str,
        profile: Option<&str>,
    ) -> Result<ModelResolutionContext, ModelResolutionError> {
        self(model, profile)
    }
}

/// Managed model-restriction context threaded into the plan-mode upgrade swap
/// (binary `RF`) and the subagent model-request gate (binary `ble`/`Qly`). Boot
/// resolves it once from the managed `availableModels` allowlist and hands it to
/// the spawner; a default install has no policy allowlist so
/// [`ModelEnforcement::Inactive`] flows through as a byte-for-byte no-op.
#[derive(Clone, Copy)]
pub struct ModelRestriction<'a> {
    /// Resolved enforcement (managed allowlist + overrides, or Inactive/Refused).
    pub enforcement: &'a ModelEnforcement,
    /// The concrete model catalog "newest permitted of family" is resolved
    /// against (binary `ykr` queries the live registry).
    pub catalog: &'a [String],
}

impl<'a> ModelRestriction<'a> {
    /// `true` when the managed restriction actively BARS `model` (binary
    /// `!(P4(x)??sl(x))` reduced to the env-free `availableModels` arm). An
    /// absent restriction, or [`ModelEnforcement::Inactive`], never bars.
    #[must_use]
    fn bars(&self, model: &str) -> bool {
        allowlist::model_allowed_under(self.enforcement, model) == Some(false)
    }

    /// The allowlist + overrides when enforcement is active (for
    /// `newest_permitted_in_family`); `None` for Inactive/Refused. The returned
    /// borrows carry the restriction's `'a` lifetime (they come from the
    /// `&'a ModelEnforcement`), not the temporary `&self`.
    fn active(&self) -> Option<(&'a [String], &'a std::collections::BTreeMap<String, String>)> {
        match self.enforcement {
            ModelEnforcement::Active {
                allowlist,
                overrides,
            } => Some((allowlist, overrides)),
            _ => None,
        }
    }
}

/// `true` when a restriction is present AND actively bars `model`.
fn restriction_bars(restriction: Option<ModelRestriction<'_>>, model: &str) -> bool {
    restriction.is_some_and(|r| r.bars(model))
}

/// Cross-region inference profile prefixes for Bedrock (`bedrock.ts:189`).
const BEDROCK_REGION_PREFIXES: [&str; 4] = ["us", "eu", "apac", "global"];

/// Extract the model/inference-profile id from a Bedrock ARN. If the input is
/// not an ARN, returns it unchanged. 1:1 with `extractModelIdFromArn`
/// (`bedrock.ts:199-208`).
///
/// ARN format: `arn:aws:bedrock:<region>:<account>:inference-profile/<profile-id>`
fn extract_model_id_from_arn(model_id: &str) -> &str {
    if !model_id.starts_with("arn:") {
        return model_id;
    }
    match model_id.rfind('/') {
        Some(i) => &model_id[i + 1..],
        None => model_id,
    }
}

/// Extract the region prefix from a Bedrock cross-region inference model id
/// (handles both plain ids and full ARN format). 1:1 with `getBedrockRegionPrefix`
/// (`bedrock.ts:222-235`).
///
/// For example:
/// - `"eu.anthropic.claude-sonnet-4-5-20250929-v1:0"` → `Some("eu")`
/// - `"us.anthropic.claude-3-7-sonnet-20250219-v1:0"` → `Some("us")`
/// - `"arn:aws:bedrock:ap-northeast-2:123:inference-profile/global.anthropic.claude-opus-4-6-v1"` → `Some("global")`
/// - `"anthropic.claude-3-5-sonnet-20241022-v2:0"` → `None` (foundation model)
/// - `"claude-sonnet-4-5-20250929"` → `None` (first-party format)
fn get_bedrock_region_prefix(model_id: &str) -> Option<&'static str> {
    let effective_model_id = extract_model_id_from_arn(model_id);
    BEDROCK_REGION_PREFIXES
        .into_iter()
        .find(|&prefix| effective_model_id.starts_with(&format!("{prefix}.anthropic.")))
}

/// `true` if a model id is a foundation model (e.g.
/// `"anthropic.claude-sonnet-4-5-20250929-v1:0"`). 1:1 with `isFoundationModel`
/// (`bedrock.ts:181-183`).
fn is_foundation_model(model_id: &str) -> bool {
    model_id.starts_with("anthropic.")
}

/// Apply a region prefix to a Bedrock model id. If the model already has a
/// different region prefix, it is replaced. If the model is a foundation model
/// (`anthropic.*`), the prefix is added. Otherwise returned as-is. 1:1 with
/// `applyBedrockRegionPrefix` (`bedrock.ts:248-265`).
///
/// For example:
/// - `applyBedrockRegionPrefix("us.anthropic.claude-sonnet-4-5-v1:0", "eu")` → `"eu.anthropic.claude-sonnet-4-5-v1:0"`
/// - `applyBedrockRegionPrefix("anthropic.claude-sonnet-4-5-v1:0", "eu")` → `"eu.anthropic.claude-sonnet-4-5-v1:0"`
/// - `applyBedrockRegionPrefix("claude-sonnet-4-5-20250929", "eu")` → `"claude-sonnet-4-5-20250929"` (not a Bedrock model)
fn apply_bedrock_region_prefix(model_id: &str, prefix: &str) -> String {
    // Check if it already has a region prefix and replace it.
    if let Some(existing) = get_bedrock_region_prefix(model_id) {
        return model_id.replacen(&format!("{existing}."), &format!("{prefix}."), 1);
    }
    // Check if it's a foundation model (anthropic.*) and add the prefix.
    if is_foundation_model(model_id) {
        return format!("{prefix}.{model_id}");
    }
    // Not a Bedrock model format, return as-is.
    model_id.to_string()
}

/// Check if a bare family alias (`opus`/`sonnet`/`haiku`) matches the parent
/// model's tier. When it does, the subagent inherits the parent's EXACT model
/// string instead of resolving the alias to a provider default. 1:1 with
/// `aliasMatchesParentTier` (`agent.ts:110-122`).
///
/// Prevents surprising downgrades: a Vertex user on Opus 4.6 (via `/model`) who
/// spawns a subagent with `model: opus` should get Opus 4.6, not whatever
/// `getDefaultOpusModel()` returns for 3P.
///
/// Only bare family aliases match. `opus[1m]`, `best`, `opusplan` fall through
/// (the default arm) since they carry semantics beyond "same tier as parent".
/// CRITICAL: it uses `getCanonicalName(parentModel)` (which strips dates / ARN /
/// provider noise), NOT a raw substring on the full id.
fn alias_matches_parent_tier(alias: &str, parent_model: &str) -> bool {
    let canonical = canonical_name(parent_model);
    match lingxi_core::host::effort::trim_js_whitespace(alias)
        .to_lowercase()
        .as_str()
    {
        "opus" => canonical.contains("opus"),
        "sonnet" => canonical.contains("sonnet"),
        "haiku" => canonical.contains("haiku"),
        _ => false,
    }
}

/// Resolve a full model id to a shorter canonical family name (ports
/// `firstPartyNameToCanonical`, `model.ts:217-270`); the
/// `resolveOverriddenModel`/Bedrock-ARN indirection (`getCanonicalName`,
/// `model.ts:279-283`) is a no-op for these substring checks, so it is folded
/// in.
fn canonical_name(model: &str) -> String {
    let name = lingxi_core::host::effort::trim_js_whitespace(model).to_lowercase();
    // Order matters: check more specific versions first (4-6 before 4-5 before 4).
    if name.contains("claude-opus-4-6") {
        return "claude-opus-4-6".to_string();
    }
    if name.contains("claude-opus-4-5") {
        return "claude-opus-4-5".to_string();
    }
    if name.contains("claude-opus-4-1") {
        return "claude-opus-4-1".to_string();
    }
    if name.contains("claude-opus-4") {
        return "claude-opus-4".to_string();
    }
    // sonnet-5 before the sonnet-4-x arms (2.1.198; mutually exclusive —
    // "claude-sonnet-4-5" does NOT contain "sonnet-5").
    if name.contains("claude-sonnet-5") {
        return "claude-sonnet-5".to_string();
    }
    if name.contains("claude-sonnet-4-6") {
        return "claude-sonnet-4-6".to_string();
    }
    if name.contains("claude-sonnet-4-5") {
        return "claude-sonnet-4-5".to_string();
    }
    if name.contains("claude-sonnet-4") {
        return "claude-sonnet-4".to_string();
    }
    if name.contains("claude-haiku-4-5") {
        return "claude-haiku-4-5".to_string();
    }
    if name.contains("claude-3-7-sonnet") {
        return "claude-3-7-sonnet".to_string();
    }
    if name.contains("claude-3-5-sonnet") {
        return "claude-3-5-sonnet".to_string();
    }
    if name.contains("claude-3-5-haiku") {
        return "claude-3-5-haiku".to_string();
    }
    if name.contains("claude-3-opus") {
        return "claude-3-opus".to_string();
    }
    if name.contains("claude-3-sonnet") {
        return "claude-3-sonnet".to_string();
    }
    if name.contains("claude-3-haiku") {
        return "claude-3-haiku".to_string();
    }
    // Fall back to the lowercased input when no pattern matches (the TS regex
    // only narrows the unmatched case; substring checks are equivalent here).
    name
}

/// Resolve a user-specified model using facts for the actual selected route.
///
/// This is the production `At(model)` boundary. Provider identity, endpoint,
/// protocol, family defaults, and Native-only catalog facts arrive from the
/// host's route-context provider; this pure resolver never reads provider env
/// switches or guesses provider kind from a profile name/URL. `Some("")` in a
/// family default is preserved as a configured empty override.
pub fn resolve_user_specified_model(
    model_input: &str,
    context: &ModelResolutionContext,
) -> Result<String, ModelResolutionError> {
    let trimmed = lingxi_core::host::effort::trim_js_whitespace(model_input);
    let normalized = trimmed.to_lowercase();
    if context.disable_1m_context && has_1m_context(&normalized) {
        // Native's `_I` is a JS-truthy gate: a disabled 1M suffix bypasses
        // alias parsing and preserves the supplied spelling.
        return Ok(trimmed.to_string());
    }

    if is_registered_concrete_model(trimmed, context) {
        return Ok(if has_1m_context(trimmed) {
            normalize_1m_suffix(trimmed)
        } else {
            trimmed.to_string()
        });
    }

    let has_1m_tag = has_1m_context(&normalized);
    let base = if has_1m_tag {
        strip_1m_suffix(&normalized)
    } else {
        normalized.clone()
    };
    let family_default = |family: &str| {
        context
            .family_defaults
            .get(family)
            .map(str::to_owned)
            .ok_or_else(|| ModelResolutionError::MissingFamilyDefault {
                family: family.to_string(),
                profile: context.route.profile.clone(),
            })
    };

    // Explicit catalog aliases own their meaning. The four family defaults
    // retain their provider-specific override policy; reserved strategies such
    // as best/opusplan apply only when the catalog has no explicit alias.
    if !matches!(base.as_str(), "opus" | "sonnet" | "haiku" | "fable") {
        if let Some(models) = context.catalog_aliases.get(&base) {
            return match models.as_slice() {
                [model] => Ok(append_1m_suffix(model.clone(), has_1m_tag)),
                _ => Err(ModelResolutionError::RouteUnavailable {
                    model: trimmed.to_string(),
                    profile: context.route.profile.clone(),
                    reason: format!(
                        "configured alias must select one model; candidates: {models:?}"
                    ),
                }),
            };
        }
    }

    match base.as_str() {
        "opusplan" | "sonnet" => Ok(append_1m_suffix(family_default("sonnet")?, has_1m_tag)),
        "haiku" => Ok(append_1m_suffix(family_default("haiku")?, has_1m_tag)),
        "opus" => Ok(append_1m_suffix(family_default("opus")?, has_1m_tag)),
        "fable" => {
            if context.fable_strategy_available == Some(false) {
                return Err(ModelResolutionError::MissingFamilyDefault {
                    family: "fable".to_string(),
                    profile: context.route.profile.clone(),
                });
            }
            // Native has an additional dynamic endpoint/catalog gate for Fable
            // and 1M. The host has no trusted source for those facts yet, so it
            // keeps an explicitly supplied suffix and does not claim parity.
            Ok(append_1m_suffix(family_default("fable")?, has_1m_tag))
        }
        "best" => {
            // The Native `kI()` strategy/catalog lookup and entitlement overlay
            // are not available in this runtime. Use a trusted host best model
            // when supplied; otherwise preserve the current host Opus fallback.
            match context.best_model.as_ref() {
                Some(model) => Ok(model.clone()),
                None => family_default("opus"),
            }
        }
        _ => {
            // Non-aliases retain caller casing; a recognized 1M suffix is
            // normalized to one canonical trailing marker.
            if has_1m_tag {
                Ok(normalize_1m_suffix(trimmed))
            } else {
                Ok(trimmed.to_string())
            }
        }
    }
}

/// Resolve the current host's user model setting using route facts. This helper
/// is also used by the live session selection provider, so boot and subsequent
/// Agent/teammate spawns share the same model/profile classification.
pub fn resolve_agent_model_with_context(
    model: &AgentModel,
    parent_model: &str,
    permission_mode: PermissionMode,
    model_setting: Option<&str>,
    context: &ModelResolutionContext,
) -> Result<String, ModelResolutionError> {
    resolve_agent_model_restricted_with_context(
        model,
        parent_model,
        permission_mode,
        model_setting,
        None,
        context,
        &mut |_| {},
    )
}

/// [`resolve_agent_model_with_context`] with the managed-model restriction.
/// Route identity and family aliases still come exclusively from `context`.
pub fn resolve_agent_model_restricted_with_context(
    model: &AgentModel,
    parent_model: &str,
    permission_mode: PermissionMode,
    model_setting: Option<&str>,
    restriction: Option<ModelRestriction<'_>>,
    context: &ModelResolutionContext,
    warn: &mut dyn FnMut(&str),
) -> Result<String, ModelResolutionError> {
    // Claude's tier strategies belong to an identified Claude provider. A
    // live route can differ from the boot model setting, and other providers
    // may define these alias names with their own catalog meaning.
    let native_tier_strategy = matches!(
        context.route.provider,
        Some(
            ModelProviderKind::FirstParty
                | ModelProviderKind::Bedrock
                | ModelProviderKind::Vertex
                | ModelProviderKind::Foundry
                | ModelProviderKind::AnthropicAws
                | ModelProviderKind::AnthropicGoogleCloud
                | ModelProviderKind::Mantle
                | ModelProviderKind::Gateway
        )
    );
    let inherit = |warn: &mut dyn FnMut(&str)| -> Result<String, ModelResolutionError> {
        let plan = native_tier_strategy
            && permission_mode == PermissionMode::Plan
            && !model_setting.is_some_and(|setting| is_registered_concrete_model(setting, context));
        if plan
            && !model_setting
                .is_some_and(|setting| setting == "opusplan" || setting == "opusplan[1m]")
        {
            if model_setting == Some("haiku") {
                let upgrade = resolve_user_specified_model("sonnet", context)?;
                if restriction_bars(restriction, &upgrade) {
                    if let Some((allow, overrides, catalog)) =
                        restriction.and_then(|r| r.active().map(|(a, o)| (a, o, r.catalog)))
                    {
                        if let Some(newest) = allowlist::newest_permitted_in_family(
                            "sonnet",
                            catalog,
                            Some(allow),
                            Some(overrides),
                        ) {
                            warn(allowlist::warnings::PLAN_HAIKU_NEWEST);
                            return Ok(newest);
                        }
                    }
                    warn(allowlist::warnings::PLAN_HAIKU_RESTING);
                    return resolve_user_specified_model("haiku", context);
                }
                return Ok(upgrade);
            }
        }
        if plan
            && (model_setting == Some("opusplan") || model_setting == Some("opusplan[1m]"))
            && !context.catalog_aliases.contains_key("opusplan")
        {
            let one_m = model_setting == Some("opusplan[1m]");
            let mut upgrade = resolve_user_specified_model("opus", context)?;
            if one_m && !upgrade.to_lowercase().ends_with("[1m]") {
                upgrade.push_str("[1m]");
            }
            if restriction_bars(restriction, &upgrade) {
                if let Some((allow, overrides, catalog)) =
                    restriction.and_then(|r| r.active().map(|(a, o)| (a, o, r.catalog)))
                {
                    if let Some(newest) = allowlist::newest_permitted_in_family(
                        "opus",
                        catalog,
                        Some(allow),
                        Some(overrides),
                    ) {
                        warn(allowlist::warnings::PLAN_OPUSPLAN_NEWEST);
                        return Ok(newest);
                    }
                }
                warn(allowlist::warnings::PLAN_OPUSPLAN_RESTING);
                return resolve_user_specified_model(model_setting.unwrap_or("opusplan"), context);
            }
            return Ok(upgrade);
        }
        Ok(parent_model.to_string())
    };

    // `LINGXI_SUBAGENT_MODEL` is a user override, not a provider identity
    // source. It keeps Native's early-return ordering and bypasses region-prefix
    // inheritance.
    if let Some(value) = std::env::var(branding::SUBAGENT_MODEL_ENV)
        .ok()
        .filter(|value| !value.is_empty())
    {
        let resolved = resolve_user_specified_model(&value, context)?;
        if restriction_bars(restriction, &resolved) {
            warn(&format!(
                "Subagent model \"{value}{}",
                allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
            ));
            return inherit(warn);
        }
        return Ok(resolved);
    }

    let parent_region_prefix = get_bedrock_region_prefix(parent_model);
    let apply_parent_region_prefix = |resolved: &str, original_spec: &str| -> String {
        if let Some(prefix) = parent_region_prefix {
            if context.route.provider == Some(ModelProviderKind::Bedrock) {
                if get_bedrock_region_prefix(original_spec).is_some() {
                    return resolved.to_owned();
                }
                return apply_bedrock_region_prefix(resolved, prefix);
            }
        }
        resolved.to_owned()
    };

    match model {
        AgentModel::Inherit => inherit(warn),
        AgentModel::Explicit(spec) | AgentModel::Alias(spec) => {
            if native_tier_strategy
                && !is_registered_concrete_model(spec, context)
                && context
                    .family_defaults
                    .get(&lingxi_core::host::effort::trim_js_whitespace(spec).to_lowercase())
                    .is_some()
                && alias_matches_parent_tier(spec, parent_model)
            {
                return Ok(parent_model.to_string());
            }
            let resolved = resolve_user_specified_model(spec, context)?;
            let resolved = apply_parent_region_prefix(&resolved, spec);
            if restriction_bars(restriction, &resolved) {
                warn(&format!(
                    "Subagent model \"{spec}{}",
                    allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
                ));
                return inherit(warn);
            }
            Ok(resolved)
        }
    }
}

fn append_1m_suffix(model: String, requested: bool) -> String {
    if requested {
        normalize_1m_suffix(&model)
    } else {
        model
    }
}

/// `true` if `model` carries an explicit `[1m]` suffix (case-insensitive).
/// Mirrors Native `lbt`'s case-insensitive substring test. The caller applies
/// Native `Xd`'s environment gate.
fn has_1m_context(model: &str) -> bool {
    model.to_lowercase().contains("[1m]")
}

/// Strip a single trailing `[1m]` (case-insensitive) and trim. Mirrors the TS
/// `replace(/\[1m]$/i, '').trim()`.
fn strip_1m_suffix(s: &str) -> String {
    if s.to_lowercase().ends_with("[1m]") {
        lingxi_core::host::effort::trim_js_whitespace(&s[..s.len() - 4]).to_string()
    } else {
        lingxi_core::host::effort::trim_js_whitespace(s).to_string()
    }
}

/// Native `e$` removes every contiguous trailing `[1m]` tag, trims, and adds
/// one canonical suffix.
fn normalize_1m_suffix(s: &str) -> String {
    let mut base = lingxi_core::host::effort::trim_js_whitespace(s).to_string();
    while base.to_lowercase().ends_with("[1m]") {
        base.truncate(base.len() - "[1m]".len());
        base = base
            .trim_end_matches(lingxi_core::host::effort::javascript_whitespace)
            .to_string();
    }
    format!("{base}[1m]")
}

/// The Explore model-cap ladder — claude-code 2.1.198 `Kyl`
/// (`["haiku","sonnet","opus"]`). `obm` slices it up to and including
/// [`EXPLORE_MODEL_CAP`] (`Kyl.slice(0, Kyl.indexOf(Yyl)+1)` — the whole array,
/// since opus is last) and asks whether the session model names ANY of these
/// families.
const EXPLORE_MODEL_CAP_LADDER: [&str; 3] = ["haiku", "sonnet", "opus"];

/// The alias the built-in Explore agent is capped at — claude-code 2.1.198
/// `Yyl` (`"opus"`).
const EXPLORE_MODEL_CAP: &str = "opus";

/// 1:1 port of `dPn(e,t)` (2.1.198): `true` iff the lowercased model string
/// contains ANY of the (non-empty) needles, case-insensitively.
fn model_contains_any(model: &str, needles: &[&str]) -> bool {
    let lower = model.to_lowercase();
    needles
        .iter()
        .any(|n| !n.is_empty() && lower.contains(&n.to_lowercase()))
}

/// 1:1 port of `obm(e)` (2.1.198): `true` iff the provider is firstParty AND
/// the session model names NONE of the haiku/sonnet/opus families (i.e. a
/// fable/mythos-class session model, "above" the opus cap).
///
/// `session_provider_first_party` is LingXi's multi-provider extension of the
/// `fr() !== "firstParty"` gate: the composition root passes `false` when the
/// session's default model routes to a non-Anthropic provider profile
/// (OpenAI/Gemini/…), which behaves exactly like the TS non-firstParty branch
/// (→ `false` → Explore inherits). The host route is authoritative.
fn session_model_exceeds_explore_cap(
    session_model: &str,
    session_provider_first_party: bool,
) -> bool {
    if !session_provider_first_party {
        return false;
    }
    let cap_idx = EXPLORE_MODEL_CAP_LADDER
        .iter()
        .position(|m| *m == EXPLORE_MODEL_CAP)
        .expect("the cap is in the ladder");
    let ladder = &EXPLORE_MODEL_CAP_LADDER[..=cap_idx];
    !model_contains_any(session_model, ladder)
}

/// 1:1 port of `GAe(e,t)` (2.1.198): the built-in `Explore` agent's model is
/// derived from the SESSION model instead of its (now `"inherit"`) frontmatter.
///
/// ```js
/// function GAe(e,t){if(e.agentType!==qme.agentType||e.source!=="built-in")return e.model;
///   return obm(t)?Yyl:"inherit"}
/// ```
///
/// - Any non-Explore or non-built-in definition: `def.model` unchanged (a
///   user/project agent literally named "Explore" keeps its own model).
/// - Built-in Explore on a firstParty session whose model names none of
///   haiku/sonnet/opus (fable/mythos-class): the `"opus"` alias — Explore
///   inherits the session model CAPPED at opus.
/// - Otherwise (haiku/sonnet/opus session, or any non-firstParty provider):
///   `"inherit"` — Explore runs on the session model.
///
/// 2.1.266 spells the same function `yX` and adds a kill-switch ahead of the
/// cap test (@1496xxx):
///
/// ```js
/// if(a.CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP)return"inherit";
/// ```
///
/// so a deployment can let Explore run on the full session model. The port
/// reads it through `is_env_truthy` rather than JS truthiness, the same
/// approximation every other bare `a.X` gate here uses — it differs only for a
/// value like `"0"`, which is truthy in JS and which nobody sets on a
/// kill-switch.
#[must_use]
pub fn resolve_builtin_explore_model(
    def: &AgentDefinition,
    session_model: &str,
    session_provider_first_party: bool,
) -> AgentModel {
    if def.agent_type != "Explore" || !matches!(def.source, AgentSource::BuiltIn) {
        return def.model.clone();
    }
    if lingxi_core::host::env::is_env_truthy(
        std::env::var("LINGXI_DISABLE_EXPLORE_INHERIT_CAP")
            .ok()
            .as_deref(),
    ) {
        return AgentModel::Inherit;
    }
    if session_model_exceeds_explore_cap(session_model, session_provider_first_party) {
        AgentModel::Alias(EXPLORE_MODEL_CAP.to_string())
    } else {
        AgentModel::Inherit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const DEFAULT: PermissionMode = PermissionMode::Default;

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.prev {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn test_context(
        profile: &str,
        model: &str,
        provider: ModelProviderKind,
    ) -> ModelResolutionContext {
        ModelResolutionContext {
            route: ModelRouteFacts {
                model: model.into(),
                profile: Some(profile.into()),
                provider: Some(provider),
                ..Default::default()
            },
            family_defaults: FamilyModelDefaults {
                opus: Some("claude-opus-4-8".into()),
                sonnet: Some("claude-sonnet-5".into()),
                haiku: Some("claude-haiku-4-5".into()),
                fable: Some("claude-fable-5-1".into()),
            },
            ..Default::default()
        }
    }

    fn resolve_test_model_restricted(
        model: &AgentModel,
        parent_model: &str,
        permission_mode: PermissionMode,
        model_setting: Option<&str>,
        restriction: Option<ModelRestriction<'_>>,
        warn: &mut dyn FnMut(&str),
    ) -> String {
        resolve_agent_model_restricted_with_context(
            model,
            parent_model,
            permission_mode,
            model_setting,
            restriction,
            &test_context("anthropic", parent_model, ModelProviderKind::FirstParty),
            warn,
        )
        .unwrap()
    }

    #[test]
    fn user_model_aliases_use_supplied_route_catalog() {
        let mut context = test_context("custom", "parent", ModelProviderKind::Other);
        context.family_defaults = FamilyModelDefaults {
            opus: Some("powerful-model".into()),
            sonnet: Some("balanced-model".into()),
            haiku: Some("fast-model".into()),
            fable: Some("large-model".into()),
        };
        for (alias, expected) in [
            ("OPUS", "powerful-model"),
            ("sonnet", "balanced-model"),
            ("haiku", "fast-model"),
            ("fable", "large-model"),
            ("opusplan", "balanced-model"),
            ("best", "powerful-model"),
        ] {
            assert_eq!(
                resolve_user_specified_model(alias, &context).unwrap(),
                expected
            );
        }
        context.best_model = Some("catalog-best".into());
        assert_eq!(
            resolve_user_specified_model("best", &context).unwrap(),
            "catalog-best"
        );
    }

    #[test]
    fn explicit_catalog_aliases_precede_reserved_model_strategies() {
        let mut context = ModelResolutionContext {
            route: ModelRouteFacts {
                model: "gpt-balanced".into(),
                profile: Some("openai".into()),
                provider: Some(ModelProviderKind::Other),
                ..Default::default()
            },
            ..Default::default()
        };
        context
            .catalog_aliases
            .insert("best".into(), vec!["gpt-best".into()]);
        context
            .catalog_aliases
            .insert("opusplan".into(), vec!["gpt-balanced".into()]);
        context
            .catalog_aliases
            .insert("fast".into(), vec!["gpt-fast".into()]);
        for (alias, expected) in [
            ("best", "gpt-best"),
            ("opusplan", "gpt-balanced"),
            ("fast", "gpt-fast"),
        ] {
            assert_eq!(
                resolve_user_specified_model(alias, &context).unwrap(),
                expected
            );
        }
        assert_eq!(
            resolve_user_specified_model("BEST[1M]", &context).unwrap(),
            "gpt-best[1m]"
        );
        context
            .catalog_aliases
            .insert("best".into(), vec!["gpt-a".into(), "gpt-b".into()]);
        assert!(matches!(
            resolve_user_specified_model("best", &context),
            Err(ModelResolutionError::RouteUnavailable { .. })
        ));
    }

    #[test]
    fn plan_tier_strategies_do_not_replace_foreign_live_routes() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        let enforcement = active(&["custom-live-model"]);
        let catalog = catalog(&["custom-live-model", "claude-sonnet-5", "claude-opus-4-8"]);
        for provider in [Some(ModelProviderKind::Other), None] {
            for defaults in [
                FamilyModelDefaults::default(),
                test_context("a", "parent", ModelProviderKind::Other).family_defaults,
            ] {
                let context = ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: "custom-live-model".into(),
                        profile: Some("custom".into()),
                        provider,
                        ..Default::default()
                    },
                    family_defaults: defaults,
                    ..Default::default()
                };
                for boot_setting in ["haiku", "opusplan", "opusplan[1m]"] {
                    let mut warnings = Vec::new();
                    let model = resolve_agent_model_restricted_with_context(
                        &AgentModel::Inherit,
                        &context.route.model,
                        PermissionMode::Plan,
                        Some(boot_setting),
                        Some(ModelRestriction {
                            enforcement: &enforcement,
                            catalog: &catalog,
                        }),
                        &context,
                        &mut |warning| warnings.push(warning.to_owned()),
                    )
                    .unwrap();
                    assert_eq!(model, "custom-live-model");
                    assert!(warnings.is_empty());
                }
            }
        }
    }

    #[test]
    fn plan_tier_strategies_preserve_identified_claude_provider_behavior() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        for provider in [
            ModelProviderKind::FirstParty,
            ModelProviderKind::Bedrock,
            ModelProviderKind::Vertex,
            ModelProviderKind::Foundry,
            ModelProviderKind::AnthropicAws,
            ModelProviderKind::AnthropicGoogleCloud,
            ModelProviderKind::Mantle,
            ModelProviderKind::Gateway,
        ] {
            let context = test_context("native", "claude-haiku-4-5", provider);
            for (setting, expected) in [
                ("haiku", "claude-sonnet-5"),
                ("opusplan", "claude-opus-4-8"),
                ("opusplan[1m]", "claude-opus-4-8[1m]"),
            ] {
                assert_eq!(
                    resolve_agent_model_with_context(
                        &AgentModel::Inherit,
                        &context.route.model,
                        PermissionMode::Plan,
                        Some(setting),
                        &context,
                    )
                    .unwrap(),
                    expected,
                );
            }
        }
    }

    #[test]
    fn foreign_family_aliases_keep_catalog_meaning_for_claude_shaped_ids() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        for provider in [Some(ModelProviderKind::Other), None] {
            for (alias, parent) in [
                ("opus", "claude-opus-4-8"),
                ("sonnet", "claude-sonnet-5"),
                ("haiku", "claude-haiku-4-5"),
            ] {
                let context = ModelResolutionContext {
                    route: ModelRouteFacts {
                        model: parent.into(),
                        profile: Some("custom".into()),
                        provider,
                        ..Default::default()
                    },
                    family_defaults: FamilyModelDefaults {
                        opus: Some("configured-powerful".into()),
                        sonnet: Some("configured-balanced".into()),
                        haiku: Some("configured-fast".into()),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                for preference in [
                    AgentModel::Alias(alias.into()),
                    AgentModel::Explicit(alias.into()),
                ] {
                    assert_eq!(
                        resolve_agent_model_with_context(
                            &preference,
                            parent,
                            PermissionMode::Plan,
                            Some("haiku"),
                            &context,
                        )
                        .unwrap(),
                        context.family_defaults.get(alias).unwrap(),
                    );
                }
            }
        }
    }

    #[test]
    fn configured_opusplan_alias_inherits_selected_model_in_plan_mode() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        let mut context = ModelResolutionContext {
            route: ModelRouteFacts {
                model: "gpt-balanced".into(),
                profile: Some("openai".into()),
                provider: Some(ModelProviderKind::Other),
                ..Default::default()
            },
            ..Default::default()
        };
        context
            .catalog_aliases
            .insert("opusplan".into(), vec!["gpt-balanced".into()]);
        assert_eq!(
            resolve_agent_model_with_context(
                &AgentModel::Inherit,
                "gpt-balanced",
                PermissionMode::Plan,
                Some("opusplan"),
                &context
            )
            .unwrap(),
            "gpt-balanced"
        );
        assert_eq!(
            resolve_agent_model_with_context(
                &AgentModel::Alias("opusplan".into()),
                "gpt-balanced",
                PermissionMode::Plan,
                None,
                &context
            )
            .unwrap(),
            "gpt-balanced"
        );
    }

    #[test]
    fn absent_family_default_errors_on_current_profile() {
        let context = ModelResolutionContext {
            route: ModelRouteFacts {
                profile: Some("openai".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            resolve_user_specified_model("sonnet", &context),
            Err(ModelResolutionError::MissingFamilyDefault {
                family: "sonnet".into(),
                profile: Some("openai".into())
            })
        );
    }

    #[test]
    fn configured_empty_family_default_is_presence_sensitive() {
        let mut context = ModelResolutionContext::default();
        context.family_defaults.fable = Some(String::new());
        assert_eq!(resolve_user_specified_model("fable", &context).unwrap(), "");
        context.fable_strategy_available = Some(false);
        assert!(matches!(
            resolve_user_specified_model("fable", &context),
            Err(ModelResolutionError::MissingFamilyDefault { .. })
        ));
    }

    #[test]
    fn model_normalization_preserves_casing_and_ecmascript_whitespace() {
        let context = test_context("a", "parent", ModelProviderKind::FirstParty);
        assert_eq!(
            resolve_user_specified_model("\u{feff}SoNnEt\u{feff}", &context).unwrap(),
            "claude-sonnet-5"
        );
        assert_eq!(
            resolve_user_specified_model("\u{0085}sonnet\u{0085}", &context).unwrap(),
            "\u{0085}sonnet\u{0085}"
        );
        assert_eq!(
            resolve_user_specified_model("Custom-Model[1m][1M]", &context).unwrap(),
            "Custom-Model[1m]"
        );
        assert_eq!(
            resolve_user_specified_model("OPUS[1M]", &context).unwrap(),
            "claude-opus-4-8[1m]"
        );
        let mut disabled = context;
        disabled.disable_1m_context = true;
        assert_eq!(
            resolve_user_specified_model("OPUS[1M]", &disabled).unwrap(),
            "OPUS[1M]"
        );
    }

    #[test]
    fn explicit_model_ids_are_preserved_for_every_provider() {
        for provider in [
            ModelProviderKind::FirstParty,
            ModelProviderKind::Bedrock,
            ModelProviderKind::Gateway,
            ModelProviderKind::Other,
        ] {
            let context = test_context("a", "parent", provider);
            assert_eq!(
                resolve_user_specified_model("claude-opus-4-1-20250805", &context).unwrap(),
                "claude-opus-4-1-20250805"
            );
            assert_eq!(
                resolve_user_specified_model("CLAUDE-OPUS-4-1-20250805[1M]", &context).unwrap(),
                "CLAUDE-OPUS-4-1-20250805[1m]"
            );
            assert_eq!(
                resolve_user_specified_model("Custom-Wire-Model", &context).unwrap(),
                "Custom-Wire-Model"
            );
        }
    }

    #[test]
    fn same_tier_alias_preserves_exact_parent_model() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        let parent = "us.anthropic.claude-opus-4-6-v1:0";
        let context = test_context("bedrock", parent, ModelProviderKind::Bedrock);
        assert_eq!(
            resolve_agent_model_with_context(
                &AgentModel::Alias("opus".into()),
                parent,
                DEFAULT,
                None,
                &context
            )
            .unwrap(),
            parent
        );
        assert!(!alias_matches_parent_tier("opus[1m]", parent));
        assert!(!alias_matches_parent_tier("best", parent));
    }

    #[test]
    fn bedrock_region_prefix_uses_selected_provider() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        let parent = "us.anthropic.claude-opus-4-6-v1:0";
        let mut context = test_context("bedrock", parent, ModelProviderKind::Bedrock);
        let requested = AgentModel::Explicit("anthropic.claude-sonnet-4-6-v1:0".into());
        assert_eq!(
            resolve_agent_model_with_context(&requested, parent, DEFAULT, None, &context).unwrap(),
            "us.anthropic.claude-sonnet-4-6-v1:0"
        );
        let pinned = AgentModel::Explicit("eu.anthropic.claude-sonnet-4-6-v1:0".into());
        assert_eq!(
            resolve_agent_model_with_context(&pinned, parent, DEFAULT, None, &context).unwrap(),
            "eu.anthropic.claude-sonnet-4-6-v1:0"
        );
        context.route.provider = Some(ModelProviderKind::Other);
        assert_eq!(
            resolve_agent_model_with_context(&requested, parent, DEFAULT, None, &context).unwrap(),
            "anthropic.claude-sonnet-4-6-v1:0"
        );
    }

    #[test]
    fn explore_model_cap_uses_host_provider_and_product_switch() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = EnvGuard::set("LINGXI_DISABLE_EXPLORE_INHERIT_CAP", "");
        let def = crate::builtins::builtin_agent_definitions()
            .into_iter()
            .find(|def| def.agent_type == "Explore")
            .unwrap();
        assert!(
            matches!(resolve_builtin_explore_model(&def, "claude-fable-5-1", true), AgentModel::Alias(model) if model == "opus")
        );
        assert!(matches!(
            resolve_builtin_explore_model(&def, "gpt-4o", false),
            AgentModel::Inherit
        ));
        assert!(matches!(
            resolve_builtin_explore_model(&def, "claude-fable-5-1", false),
            AgentModel::Inherit
        ));
        assert!(matches!(
            resolve_builtin_explore_model(&def, "claude-opus-4-8", true),
            AgentModel::Inherit
        ));
        let _disabled = EnvGuard::set("LINGXI_DISABLE_EXPLORE_INHERIT_CAP", "1");
        assert!(matches!(
            resolve_builtin_explore_model(&def, "claude-fable-5-1", true),
            AgentModel::Inherit
        ));
    }

    fn skill_route_provider(
        model: &str,
        profile: Option<&str>,
    ) -> Result<ModelResolutionContext, ModelResolutionError> {
        let (qualifier, requested) = model
            .split_once('/')
            .map_or((None, model), |(profile, model)| (Some(profile), model));
        if profile
            .zip(qualifier)
            .is_some_and(|(profile, qualifier)| profile != qualifier)
        {
            return Err(ModelResolutionError::RouteUnavailable {
                model: model.into(),
                profile: profile.map(str::to_owned),
                reason: "conflicting profile qualifier".into(),
            });
        }
        let profile = profile.or(qualifier).unwrap_or("a");
        let one_m = has_1m_context(requested);
        let requested = strip_1m_suffix(requested);
        let model = match requested.as_str() {
            "parent" | "target" => requested,
            "balanced" => "target".into(),
            _ => {
                return Err(ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: Some(profile.into()),
                    reason: "model absent".into(),
                });
            }
        };
        Ok(ModelResolutionContext {
            route: ModelRouteFacts {
                model: append_1m_suffix(model, one_m),
                profile: Some(profile.into()),
                provider: Some(ModelProviderKind::Other),
                endpoint: Some(format!("https://{profile}.example")),
                protocol: Some("custom".into()),
            },
            family_defaults: FamilyModelDefaults {
                sonnet: Some("target".into()),
                ..Default::default()
            },
            catalog_aliases: [("balanced".into(), vec!["target".into()])].into(),
            native_1m: Some(true),
            ..Default::default()
        })
    }

    #[test]
    fn skill_model_selection_inherits_1m_only_with_trusted_same_route_support() {
        let parent = skill_route_provider("parent[1m]", Some("a")).unwrap();
        for preference in ["balanced", "sonnet", "target"] {
            let selected =
                resolve_skill_model_selection(preference, None, &parent, &skill_route_provider)
                    .unwrap();
            assert_eq!(selected.model, "target[1m]");
            assert_eq!(selected.model_profile.as_deref(), Some("a"));
        }
        for support in [None, Some(false)] {
            let provider = |model: &str, profile: Option<&str>| {
                let mut context = skill_route_provider(model, profile)?;
                context.native_1m = support;
                Ok(context)
            };
            let selected =
                resolve_skill_model_selection("target", None, &parent, &provider).unwrap();
            assert_eq!(selected.model, "target");
        }
        let provider = |model: &str, profile: Option<&str>| {
            let mut context = skill_route_provider(model, profile)?;
            context.disable_1m_context = true;
            Ok(context)
        };
        let selected = resolve_skill_model_selection("target", None, &parent, &provider).unwrap();
        assert_eq!(selected.model, "target");
        let mut disabled_parent = parent.clone();
        disabled_parent.disable_1m_context = true;
        assert_eq!(
            resolve_skill_model_selection("target", None, &disabled_parent, &skill_route_provider)
                .unwrap()
                .model,
            "target"
        );
    }

    #[test]
    fn skill_model_selection_cross_profile_keeps_target_route_without_inherited_1m() {
        let parent = skill_route_provider("parent[1m]", Some("a")).unwrap();
        for (model, profile) in [("b/target", None), ("target", Some("b"))] {
            let selected =
                resolve_skill_model_selection(model, profile, &parent, &skill_route_provider)
                    .unwrap();
            assert_eq!(selected.model, "target");
            assert_eq!(selected.model_profile.as_deref(), Some("b"));
        }
        let selected =
            resolve_skill_model_selection("b/target[1m]", None, &parent, &skill_route_provider)
                .unwrap();
        assert_eq!(selected.model, "target[1m]");
        assert_eq!(selected.model_profile.as_deref(), Some("b"));
    }

    #[test]
    fn skill_model_selection_does_not_infer_provider_from_matching_kind() {
        let parent = skill_route_provider("parent[1m]", Some("a")).unwrap();
        let provider = |model: &str, profile: Option<&str>| {
            let mut context = skill_route_provider(model, profile)?;
            context.route.endpoint = Some("https://different.example".into());
            Ok(context)
        };
        assert_eq!(
            resolve_skill_model_selection("target", None, &parent, &provider)
                .unwrap()
                .model,
            "target"
        );
        let mut unscoped_parent = parent;
        unscoped_parent.route.profile = None;
        unscoped_parent.route.endpoint = None;
        let unscoped_provider = |model: &str, profile: Option<&str>| {
            let mut context = skill_route_provider(model, profile)?;
            context.route.profile = None;
            context.route.endpoint = None;
            Ok(context)
        };
        assert_eq!(
            resolve_skill_model_selection("target", None, &unscoped_parent, &unscoped_provider)
                .unwrap()
                .model,
            "target"
        );
    }

    #[test]
    fn skill_model_selection_invalid_route_returns_error_without_a_model_fallback() {
        let parent = skill_route_provider("parent", Some("a")).unwrap();
        assert!(matches!(
            resolve_skill_model_selection("missing", None, &parent, &skill_route_provider),
            Err(ModelResolutionError::RouteUnavailable { .. })
        ));
    }

    fn two_profile_provider(
        model: &str,
        profile: Option<&str>,
    ) -> Result<ModelResolutionContext, ModelResolutionError> {
        let (qualified_profile, bare) = model
            .split_once('/')
            .map_or((None, model), |(profile, bare)| (Some(profile), bare));
        if profile
            .zip(qualified_profile)
            .is_some_and(|(profile, qualifier)| profile != qualifier)
        {
            return Err(ModelResolutionError::RouteUnavailable {
                model: model.into(),
                profile: profile.map(str::to_owned),
                reason: "conflicting profile qualifier".into(),
            });
        }
        let profile = profile.or(qualified_profile);
        if profile.is_some_and(|profile| profile != "a" && profile != "b") {
            return Err(ModelResolutionError::RouteUnavailable {
                model: model.into(),
                profile: profile.map(str::to_owned),
                reason: "unknown profile".into(),
            });
        }
        let target = match (bare, profile) {
            ("shared", None) => {
                return Err(ModelResolutionError::AmbiguousRoute {
                    model: bare.into(),
                    profiles: vec!["a".into(), "b".into()],
                });
            }
            ("shared", Some(profile)) => profile,
            ("only-b", None | Some("b")) => "b",
            ("only-a", None | Some("a")) => "a",
            _ => {
                return Err(ModelResolutionError::RouteUnavailable {
                    model: model.into(),
                    profile: profile.map(str::to_owned),
                    reason: "model absent".into(),
                });
            }
        };
        Ok(test_context(target, bare, ModelProviderKind::Other))
    }

    #[test]
    fn registered_native_alias_names_keep_their_concrete_route() {
        for wire_model in ["opus", "sonnet", "haiku", "fable", "best", "opusplan"] {
            let provider = |model: &str, profile: Option<&str>| {
                let qualified = format!("custom/{wire_model}");
                let selected = match (model, profile) {
                    (model, None) if model == wire_model || model == qualified => "custom",
                    (model, Some("custom")) if model == wire_model => "custom",
                    _ => {
                        return Err(ModelResolutionError::RouteUnavailable {
                            model: model.into(),
                            profile: profile.map(str::to_owned),
                            reason: "model is not registered on this profile".into(),
                        })
                    }
                };
                let mut context = test_context(selected, wire_model, ModelProviderKind::Other);
                context.registered_model_ids.insert(wire_model.into());
                // A conflicting default must not replace an actual wire ID.
                context.family_defaults = FamilyModelDefaults {
                    opus: Some("different-opus".into()),
                    sonnet: Some("different-sonnet".into()),
                    haiku: Some("different-haiku".into()),
                    fable: Some("different-fable".into()),
                };
                Ok(context)
            };
            let parent = ModelResolutionContext::default();
            let qualified = format!("custom/{wire_model}");
            for (model, profile) in [
                (wire_model, None),
                (wire_model, Some("custom")),
                (qualified.as_str(), None),
            ] {
                let selected =
                    resolve_user_model_selection(model, profile, &parent, &provider).unwrap();
                assert_eq!(selected.model, wire_model);
                assert_eq!(selected.model_profile.as_deref(), Some("custom"));
            }
            let context = provider(wire_model, Some("custom")).unwrap();
            assert_eq!(
                resolve_user_specified_model(wire_model, &context).unwrap(),
                wire_model
            );
        }
    }

    #[test]
    fn changed_relative_alias_retains_parent_profile_among_duplicate_models() {
        let mut parent = test_context("a", "only-a", ModelProviderKind::Other);
        parent.family_defaults.sonnet = Some("shared".into());
        let selection =
            resolve_user_model_selection("sonnet", None, &parent, &two_profile_provider).unwrap();
        assert_eq!(selection.model, "shared");
        assert_eq!(selection.model_profile.as_deref(), Some("a"));
    }

    #[test]
    fn native_plan_inheritance_preserves_registered_literal_strategy_names() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        for (setting, alias_upgrade) in [
            ("haiku", "claude-sonnet-5"),
            ("opusplan", "claude-opus-4-8"),
            ("opusplan[1m]", "claude-opus-4-8[1m]"),
        ] {
            let mut context = test_context("native", setting, ModelProviderKind::FirstParty);
            context
                .registered_model_ids
                .insert(strip_1m_suffix(setting));
            assert_eq!(
                resolve_agent_model_with_context(
                    &AgentModel::Inherit,
                    setting,
                    PermissionMode::Plan,
                    Some(setting),
                    &context,
                )
                .unwrap(),
                setting,
            );
            context.registered_model_ids.clear();
            assert_eq!(
                resolve_agent_model_with_context(
                    &AgentModel::Inherit,
                    setting,
                    PermissionMode::Plan,
                    Some(setting),
                    &context,
                )
                .unwrap(),
                alias_upgrade,
            );
        }
    }

    #[test]
    fn native_same_tier_does_not_replace_a_registered_literal_model() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        let mut context = test_context("native", "claude-sonnet-5", ModelProviderKind::FirstParty);
        context.registered_model_ids.insert("sonnet".into());
        context.family_defaults.sonnet = Some("different-sonnet".into());
        for preference in [
            AgentModel::Alias("sonnet".into()),
            AgentModel::Explicit("sonnet".into()),
        ] {
            assert_eq!(
                resolve_agent_model_with_context(
                    &preference,
                    "claude-sonnet-5",
                    PermissionMode::Default,
                    None,
                    &context,
                )
                .unwrap(),
                "sonnet"
            );
        }
    }

    #[test]
    fn unscoped_relative_alias_uses_a_unique_host_catalog() {
        let provider = |model: &str, profile: Option<&str>| {
            if (model == "sonnet" && profile.is_none())
                || (model == "configured-sonnet" && profile == Some("configured"))
            {
                let mut context = test_context("configured", model, ModelProviderKind::Other);
                context.family_defaults.sonnet = Some("configured-sonnet".into());
                return Ok(context);
            }
            Err(ModelResolutionError::RouteUnavailable {
                model: model.into(),
                profile: profile.map(str::to_owned),
                reason: "model not available".into(),
            })
        };
        let selection = resolve_user_model_selection(
            "sonnet",
            None,
            &ModelResolutionContext::default(),
            &provider,
        )
        .unwrap();
        assert_eq!(selection.model, "configured-sonnet");
        assert_eq!(selection.model_profile.as_deref(), Some("configured"));
    }

    #[test]
    fn concrete_model_prefers_parent_route_and_can_switch_to_unique_other_route() {
        let parent = test_context("a", "only-a", ModelProviderKind::Other);
        let same =
            resolve_user_model_selection("shared", None, &parent, &two_profile_provider).unwrap();
        assert_eq!(same.model_profile.as_deref(), Some("a"));
        let foreign =
            resolve_user_model_selection("only-b", None, &parent, &two_profile_provider).unwrap();
        assert_eq!(foreign.model, "only-b");
        assert_eq!(foreign.model_profile.as_deref(), Some("b"));
    }

    #[test]
    fn qualified_model_selects_its_profile_and_wire_model() {
        let parent = test_context("a", "only-a", ModelProviderKind::Other);
        let selection =
            resolve_user_model_selection("b/shared", None, &parent, &two_profile_provider).unwrap();
        assert_eq!(selection.model, "shared");
        assert_eq!(selection.model_profile.as_deref(), Some("b"));
        let pinned =
            resolve_user_model_selection("shared", Some("b"), &parent, &two_profile_provider)
                .unwrap();
        assert_eq!(pinned.model_profile.as_deref(), Some("b"));
        assert!(
            resolve_user_model_selection("only-b", Some("a"), &parent, &two_profile_provider)
                .is_err()
        );
    }

    #[test]
    fn native_slash_model_keeps_selected_profile_when_catalogs_overlap() {
        let parent = test_context("a", "vendor/shared", ModelProviderKind::Other);
        let provider = |model: &str, profile: Option<&str>| {
            if model != "vendor/shared" {
                return two_profile_provider(model, profile);
            }
            match profile {
                Some(profile) => Ok(test_context(profile, model, ModelProviderKind::Other)),
                None => Err(ModelResolutionError::AmbiguousRoute {
                    model: model.into(),
                    profiles: vec!["a".into(), "b".into()],
                }),
            }
        };
        let selected =
            resolve_user_model_selection("vendor/shared", None, &parent, &provider).unwrap();
        assert_eq!(selected.model, "vendor/shared");
        assert_eq!(selected.model_profile.as_deref(), Some("a"));
    }

    #[test]
    fn same_tier_spelling_requires_configured_family_alias() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _env = clear_subagent_env();
        let context = ModelResolutionContext {
            route: ModelRouteFacts {
                model: "claude-sonnet-4-5".into(),
                profile: Some("openai".into()),
                provider: Some(ModelProviderKind::Other),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            resolve_agent_model_with_context(
                &AgentModel::Alias("sonnet".into()),
                &context.route.model,
                DEFAULT,
                None,
                &context
            ),
            Err(ModelResolutionError::MissingFamilyDefault { .. })
        ));
    }

    #[test]
    fn missing_relative_alias_does_not_select_another_profile() {
        let mut parent = test_context("a", "only-a", ModelProviderKind::Other);
        parent.family_defaults.sonnet = None;
        assert!(matches!(
            resolve_user_model_selection("sonnet", None, &parent, &two_profile_provider),
            Err(ModelResolutionError::MissingFamilyDefault { .. })
        ));
        let no_profile = ModelResolutionContext::default();
        assert!(matches!(
            resolve_user_model_selection("shared", None, &no_profile, &two_profile_provider),
            Err(ModelResolutionError::AmbiguousRoute { .. })
        ));
    }

    use std::collections::BTreeMap;

    /// An Active enforcement over `allow` with no overrides.
    fn active(allow: &[&str]) -> ModelEnforcement {
        ModelEnforcement::Active {
            allowlist: allow.iter().map(|s| (*s).to_string()).collect(),
            overrides: BTreeMap::new(),
        }
    }

    fn catalog(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    /// Guard clearing LINGXI_SUBAGENT_MODEL so the env override never shadows the
    /// restricted-resolution tests (restored on drop).
    fn clear_subagent_env() -> EnvGuard {
        let g = EnvGuard {
            key: "LINGXI_SUBAGENT_MODEL",
            prev: std::env::var(branding::SUBAGENT_MODEL_ENV).ok(),
        };
        std::env::remove_var("LINGXI_SUBAGENT_MODEL");
        g
    }

    #[test]
    fn subagent_disallowed_model_inherits_parent_with_exact_warning() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // Allowlist permits only opus; the request resolves to Sonnet (barred).
        let enf = active(&["opus"]);
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Alias("sonnet".to_string()),
            "claude-opus-4-8",
            DEFAULT,
            None,
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        // Falls back to the parent (runtime main-loop) model.
        assert_eq!(out, "claude-opus-4-8");
        assert_eq!(
            warns,
            vec![format!(
                "Subagent model \"sonnet{}",
                allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
            )]
        );
    }

    #[test]
    fn subagent_allowed_model_passes_through_no_warning() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // Sonnet IS allowed → the request resolves normally with no warning.
        let enf = active(&["opus", "sonnet"]);
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Alias("sonnet".to_string()),
            "claude-opus-4-8",
            DEFAULT,
            None,
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-sonnet-5");
        assert!(warns.is_empty());
    }

    #[test]
    fn subagent_env_override_disallowed_inherits_with_env_value_in_warning() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = EnvGuard::set("LINGXI_SUBAGENT_MODEL", "sonnet");
        let enf = active(&["opus"]);
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Inherit,
            "claude-opus-4-8",
            DEFAULT,
            None,
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-opus-4-8");
        // The warning names the raw env value.
        assert_eq!(
            warns,
            vec![format!(
                "Subagent model \"sonnet{}",
                allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
            )]
        );
    }

    #[test]
    fn inactive_restriction_is_a_no_op() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        let enf = ModelEnforcement::Inactive;
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        // Identical to the unrestricted resolution: Alias("sonnet") → default id.
        let out = resolve_test_model_restricted(
            &AgentModel::Alias("sonnet".to_string()),
            "claude-opus-4-8",
            DEFAULT,
            None,
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-sonnet-5");
        assert!(warns.is_empty());
    }

    #[test]
    fn plan_opusplan_barred_uses_newest_permitted_opus() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // The opus upgrade (claude-opus-4-8) is barred; 4-6 is permitted.
        let enf = active(&["opus-4-6"]);
        let cat = catalog(&["claude-opus-4-6", "claude-opus-4-8"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Inherit,
            "claude-sonnet-5",
            PermissionMode::Plan,
            Some("opusplan"),
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-opus-4-6");
        assert_eq!(
            warns,
            vec![allowlist::warnings::PLAN_OPUSPLAN_NEWEST.to_string()]
        );
    }

    #[test]
    fn plan_opusplan_barred_no_permitted_opus_uses_resting_model() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // No opus is permitted at all → the resting model (opusplan → Sonnet).
        let enf = active(&["sonnet"]);
        let cat = catalog(&["claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Inherit,
            "claude-sonnet-5",
            PermissionMode::Plan,
            Some("opusplan"),
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        // Resting model = opusplan resolved normally = the Sonnet default.
        assert_eq!(out, "claude-sonnet-5");
        assert_eq!(
            warns,
            vec![allowlist::warnings::PLAN_OPUSPLAN_RESTING.to_string()]
        );
    }

    #[test]
    fn plan_haiku_barred_uses_newest_permitted_sonnet() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // The haiku plan upgrade (Sonnet default claude-sonnet-5) is barred; an
        // older permitted sonnet exists.
        let enf = active(&["sonnet-4-5"]);
        let cat = catalog(&["claude-sonnet-4-5-20250929", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Inherit,
            "claude-opus-4-8",
            PermissionMode::Plan,
            Some("haiku"),
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-sonnet-4-5-20250929");
        assert_eq!(
            warns,
            vec![allowlist::warnings::PLAN_HAIKU_NEWEST.to_string()]
        );
    }

    #[test]
    fn plan_haiku_barred_no_permitted_sonnet_uses_resting_haiku() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // No sonnet is permitted → the resting model for `haiku` = Haiku default.
        let enf = active(&["haiku"]);
        let cat = catalog(&["claude-haiku-4-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Inherit,
            "claude-opus-4-8",
            PermissionMode::Plan,
            Some("haiku"),
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-haiku-4-5");
        assert_eq!(
            warns,
            vec![allowlist::warnings::PLAN_HAIKU_RESTING.to_string()]
        );
    }

    #[test]
    fn plan_opusplan_permitted_upgrade_uses_opus_no_warning() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _s = clear_subagent_env();
        // The opus upgrade IS permitted → no substitution, no warning.
        let enf = active(&["opus"]);
        let cat = catalog(&["claude-opus-4-8"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_test_model_restricted(
            &AgentModel::Inherit,
            "claude-sonnet-5",
            PermissionMode::Plan,
            Some("opusplan"),
            Some(restriction),
            &mut |m| warns.push(m.to_string()),
        );
        assert_eq!(out, "claude-opus-4-8");
        assert!(warns.is_empty());
    }
}
