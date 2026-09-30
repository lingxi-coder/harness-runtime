//! Resolve an agent definition's model preference to a concrete wire model id.
//!
//! 1:1 port of claude-code's `getAgentModel` (`src/utils/model/agent.ts:37-95`)
//! plus the helpers it leans on (`getRuntimeMainLoopModel`, `getDefault*Model`,
//! `aliasMatchesParentTier`, `getCanonicalName`, `parseUserSpecifiedModel`, the
//! Bedrock region-prefix helpers, and `getAPIProvider`'s Bedrock arm). Without
//! this, the runner's `resolve_model` emits the bare definition string
//! (`"inherit"` / `"haiku"` / `"sonnet"`), which is not a valid Anthropic model
//! id, so built-in subagent spawns error at the provider on a default install
//! (the provider router only substitutes configured `routing.aliases`). See
//! [`resolve_agent_model`].
//!
//! ## Precedence (`getAgentModel`, agent.ts:43-94)
//! 1. **`LINGXI_SUBAGENT_MODEL`** env override (HIGHEST) → resolved via
//!    `parseUserSpecifiedModel`, bypassing the Bedrock region prefix (the TS
//!    early-return at agent.ts:43-45 returns before `applyParentRegionPrefix`).
//!    The TS guard `if (process.env.LINGXI_SUBAGENT_MODEL)` is falsy for
//!    BOTH unset and empty-string, hence the `.filter(|s| !s.is_empty())`.
//! 2. **`toolSpecifiedModel`** (agent.ts:70-76) — NOT separately ported here.
//!    The LingXi spawn path (`handle.rs::spawn`) already converts AgentTool's
//!    per-call `model` field into `AgentModel::Alias` and passes it as the
//!    `model` arg, so the alias/tier logic below covers it identically. This is
//!    the one intentional structural deviation from the TS line-for-line shape.
//! 3. **`Inherit`** (agent.ts:78-88) → `getRuntimeMainLoopModel` so an agent on
//!    `inherit` gets `opusplan`→Opus / `haiku`→Sonnet runtime resolution in plan
//!    mode (else the parent model unchanged).
//! 4. **alias / explicit** (agent.ts:90-94) → if the bare family alias matches
//!    the parent's tier (`aliasMatchesParentTier` via `getCanonicalName`)
//!    inherit the parent's EXACT id (no surprising downgrade); else
//!    `applyParentRegionPrefix(parseUserSpecifiedModel(spec), spec)`.
//!
//! ## Bedrock cross-region prefix inheritance (agent.ts:50-67)
//! When the parent model carries a cross-region inference prefix (`eu.`, `us.`,
//! …) and the provider is Bedrock, alias/explicit-resolved ids inherit that
//! prefix — UNLESS the original spec already pins its own region prefix (then it
//! is preserved, to avoid silent data-residency violations).
//!
//! ## Documented smaller deferrals (not modeled here)
//! - **Live session model**: the parent model is the BOOT-time configured model
//!   (a snapshot threaded from `cfg.model`), not the live session model — so a
//!   mid-session `/model` switch is not reflected in subsequently-spawned
//!   subagents. claude-code threads the live `parentModel`.
//! - **Nested spawns**: the parent model is always the main-loop model, not the
//!   immediate parent subagent's (the frozen request carries no parent model).
//! - **`toolSpecifiedModel` double-handle**: see precedence note 2 above — the
//!   spawn path maps AgentTool's per-call model into `AgentModel::Alias` before
//!   this fn, so the alias tier/default logic covers it; a future change that
//!   threads the per-call model straight in as a separate arg must NOT run the
//!   alias logic twice.

use crate::definition::{AgentDefinition, AgentModel, AgentSource};
use lingxi_core::host::env::is_env_truthy;
use llm_runtime::model::allowlist::{self, ModelEnforcement};
use permission::PermissionMode;

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

/// Canonical concrete id for a bare family alias.
///
/// Mirrors the current Rust catalog (orchestrator `DEFAULT_MODEL` +
/// `handle_impl` `available_models`); claude-code sources these from
/// `getModelStrings()`. DRIFT NOTE: keep aligned with those (and with
/// `tools/skill/src/model_override.rs`) when the default model versions change.
fn family_default_id(family_lower: &str) -> Option<&'static str> {
    match family_lower {
        "opus" => Some("claude-opus-4-8"),
        // 2.1.198 alias table: sonnet.default = "claude-sonnet-5" (was 4-6).
        "sonnet" => Some("claude-sonnet-5"),
        "haiku" => Some("claude-haiku-4-5"),
        _ => None,
    }
}

/// The Sonnet family default id for non-firstParty (3P) providers.
///
/// `getDefaultSonnetModel()` (`model.ts:119-128`) returns `getModelStrings().sonnet45`
/// — canonical first-party id `claude-sonnet-4-5-20250929` (`configs.ts:45`) — when
/// `getAPIProvider() !== 'firstParty'` (Bedrock/Vertex/Foundry), since those
/// providers may not yet have Sonnet 5. firstParty is on `claude-sonnet-5`
/// (`family_default_id("sonnet")`; 2.1.198 alias table
/// `sonnet.per_provider = {bedrock/vertex/foundry: "claude-sonnet-4-5"}`).
///
/// Haiku does NOT diverge by provider (`getDefaultHaikuModel` has no provider
/// branch — Haiku 4.5 is on all platforms). Opus diverges only for Foundry as
/// of 2.1.207 (Bedrock/Vertex joined the 4-8 default) — see
/// [`OPUS_FOUNDRY_DEFAULT_ID`] / [`get_default_opus_model`].
const SONNET_3P_DEFAULT_ID: &str = "claude-sonnet-4-5-20250929";

/// The Opus family default id for the Foundry provider. In the 2.1.207 alias
/// table Opus moved to per-provider ids: `opus.default = claude-opus-4-8` with
/// `per_provider = {bedrock: "claude-opus-4-8", vertex: "claude-opus-4-8",
/// foundry: "claude-opus-4-6", mantle: "claude-opus-4-8", anthropic_aws:
/// "claude-opus-4-8", gateway: "claude-opus-4-7"}`. So Bedrock/Vertex now match
/// the first-party 4-8 default and ONLY Foundry stays on 4-6 (verified against
/// the 2.1.207 binary; the changelog names only Bedrock/Vertex/Claude-on-AWS).
/// The port detects only Bedrock/Vertex/Foundry via the `CLAUDE_CODE_USE_*` env
/// vars — mantle/anthropic_aws/gateway have no runtime representation and all
/// resolve to the 4-8 default anyway (except gateway 4-7, undetected).
const OPUS_FOUNDRY_DEFAULT_ID: &str = "claude-opus-4-6";

/// `getDefaultOpusModel()` (`db()`/`nJe()` in 2.1.207): the
/// `ANTHROPIC_DEFAULT_OPUS_MODEL` env override (when non-empty) wins; else the
/// 2.1.207 alias table resolves `COn("opus", provider) ?? opus48`. Only Foundry
/// (`OPUS_FOUNDRY_DEFAULT_ID` = `claude-opus-4-6`) diverges from the 4-8 default;
/// firstParty/Bedrock/Vertex all get `claude-opus-4-8`.
fn get_default_opus_model() -> String {
    if let Some(v) = std::env::var("ANTHROPIC_DEFAULT_OPUS_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return v;
    }
    if api_provider_is_foundry() {
        return OPUS_FOUNDRY_DEFAULT_ID.to_string();
    }
    family_default_id("opus")
        .expect("opus is a known family")
        .to_string()
}

/// `getDefaultSonnetModel()` (`model.ts:118-128`): the
/// `ANTHROPIC_DEFAULT_SONNET_MODEL` env override (when non-empty) wins; else the
/// default is provider-aware — `claude-sonnet-4-5-20250929` for non-firstParty
/// (Bedrock/Vertex/Foundry, which lag), `claude-sonnet-5` for firstParty
/// (2.1.198 alias table).
fn get_default_sonnet_model() -> String {
    if let Some(v) = std::env::var("ANTHROPIC_DEFAULT_SONNET_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return v;
    }
    if api_provider_is_first_party() {
        return family_default_id("sonnet")
            .expect("sonnet is a known family")
            .to_string();
    }
    SONNET_3P_DEFAULT_ID.to_string()
}

/// `getDefaultHaikuModel()` (`model.ts:130-138`): env override, else default.
/// Haiku 4.5 is available on all platforms (firstParty/Foundry/Bedrock/Vertex),
/// so the TS has no provider branch — collapsed here.
fn get_default_haiku_model() -> String {
    env_default("ANTHROPIC_DEFAULT_HAIKU_MODEL", "haiku")
}

/// Shared body for the env-override `getDefault*Model` helpers whose defaults do
/// NOT diverge by provider (opus / haiku): env override (when non-empty) wins,
/// else the family default id. Sonnet is provider-aware — see
/// [`get_default_sonnet_model`] — so it does NOT route through here.
fn env_default(env_var: &str, family_lower: &str) -> String {
    if let Some(v) = std::env::var(env_var).ok().filter(|s| !s.is_empty()) {
        return v;
    }
    family_default_id(family_lower)
        .expect("env_default called with a known family")
        .to_string()
}

/// `getRuntimeMainLoopModel` (`model.ts:145-167`): in plan mode, `opusplan`
/// resolves to Opus (without `[1m]`) and `haiku` resolves to Sonnet; otherwise
/// the main-loop model unchanged.
///
/// NOTE the TS keys off `getUserSpecifiedModelSetting()` — the RAW user setting
/// string (e.g. `'opusplan'` / `'haiku'`) — which is why `model_setting` is
/// threaded SEPARATELY from `main_loop_model` (the resolved id). Without it the
/// Inherit branch is byte-identical to returning the parent model.
fn get_runtime_main_loop_model(
    permission_mode: PermissionMode,
    main_loop_model: &str,
    exceeds_200k_tokens: bool,
    model_setting: Option<&str>,
) -> String {
    get_runtime_main_loop_model_restricted(
        permission_mode,
        main_loop_model,
        exceeds_200k_tokens,
        model_setting,
        None,
        &mut |_| {},
    )
}

/// [`get_runtime_main_loop_model`] with the managed model-restriction gate the
/// binary `RF` applies to the plan-mode upgrade model. When the `opusplan`→Opus
/// (or `haiku`→Sonnet) upgrade model is BARRED by the managed allowlist, the
/// upgrade is replaced by the newest permitted model of that family (binary
/// `j5`), or — when nothing in the family is permitted — by the resting model
/// (the setting resolved normally). Each substitution emits its byte-exact
/// warning through `warn` (the caller de-duplicates, mirroring the binary `SN`
/// set). With `restriction == None` (or an inactive one) this is byte-identical
/// to the unrestricted resolution.
fn get_runtime_main_loop_model_restricted(
    permission_mode: PermissionMode,
    main_loop_model: &str,
    exceeds_200k_tokens: bool,
    model_setting: Option<&str>,
    restriction: Option<ModelRestriction<'_>>,
    warn: &mut dyn FnMut(&str),
) -> String {
    let plan = permission_mode == PermissionMode::Plan;

    // opusplan (with or without an explicit [1m] tag) upgrades to Opus in plan
    // mode unless the context already exceeds 200k tokens.
    let is_opusplan = model_setting == Some("opusplan") || model_setting == Some("opusplan[1m]");
    if is_opusplan && plan && !exceeds_200k_tokens {
        let one_m = model_setting == Some("opusplan[1m]");
        let upgrade = if one_m {
            format!("{}[1m]", get_default_opus_model())
        } else {
            get_default_opus_model()
        };
        if restriction_bars(restriction, &upgrade) {
            if let Some((allow, ovr, catalog)) =
                restriction.and_then(|r| r.active().map(|(a, o)| (a, o, r.catalog)))
            {
                if let Some(newest) =
                    allowlist::newest_permitted_in_family("opus", catalog, Some(allow), Some(ovr))
                {
                    warn(allowlist::warnings::PLAN_OPUSPLAN_NEWEST);
                    return newest;
                }
            }
            warn(allowlist::warnings::PLAN_OPUSPLAN_RESTING);
            // The resting model = the raw setting resolved normally (opusplan →
            // Sonnet), preserving the [1m] tag the setting carried.
            return parse_user_specified_model(model_setting.unwrap_or("opusplan"));
        }
        return upgrade;
    }

    // haiku plan setting upgrades to Sonnet in plan mode.
    if model_setting == Some("haiku") && plan {
        let upgrade = get_default_sonnet_model();
        if restriction_bars(restriction, &upgrade) {
            if let Some((allow, ovr, catalog)) =
                restriction.and_then(|r| r.active().map(|(a, o)| (a, o, r.catalog)))
            {
                if let Some(newest) =
                    allowlist::newest_permitted_in_family("sonnet", catalog, Some(allow), Some(ovr))
                {
                    warn(allowlist::warnings::PLAN_HAIKU_NEWEST);
                    return newest;
                }
            }
            warn(allowlist::warnings::PLAN_HAIKU_RESTING);
            // Resting model for the `haiku` setting = Haiku.
            return parse_user_specified_model("haiku");
        }
        return upgrade;
    }

    main_loop_model.to_string()
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

/// `getAPIProvider() === 'bedrock'` (`providers.ts:6-13`): only the Bedrock arm
/// is needed for the region-prefix seam (vertex/foundry never apply the region
/// prefix). Uses the strict-allowlist `is_env_truthy`.
fn api_provider_is_bedrock() -> bool {
    is_env_truthy(std::env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
}

/// `getAPIProvider() === 'foundry'`: the 2.1.207 precedence chain is
/// `bedrock > foundry > anthropicAws > mantle > vertex > firstParty` (verified
/// against the binary), so the provider is Foundry iff `CLAUDE_CODE_USE_BEDROCK`
/// is falsy AND `CLAUDE_CODE_USE_FOUNDRY` is env-truthy (Foundry outranks
/// Vertex/firstParty). This is the sole provider whose Opus default (4-6)
/// diverges from the 4-8 alias-table default.
fn api_provider_is_foundry() -> bool {
    !is_env_truthy(std::env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
        && is_env_truthy(std::env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref())
}

/// `getAPIProvider() === 'firstParty'` (`providers.ts:6-13`): the falsy tail of
/// the Bedrock > Vertex > Foundry > firstParty precedence chain — first-party iff
/// none of `CLAUDE_CODE_USE_BEDROCK` / `_VERTEX` / `_FOUNDRY` is env-truthy. Uses
/// the strict-allowlist `is_env_truthy` (same as the TS `isEnvTruthy`), so a
/// non-allowlisted value (e.g. `"0"` / `"false"`) does NOT switch providers.
fn api_provider_is_first_party() -> bool {
    !is_env_truthy(std::env::var("CLAUDE_CODE_USE_BEDROCK").ok().as_deref())
        && !is_env_truthy(std::env::var("CLAUDE_CODE_USE_VERTEX").ok().as_deref())
        && !is_env_truthy(std::env::var("CLAUDE_CODE_USE_FOUNDRY").ok().as_deref())
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
    match alias.to_lowercase().as_str() {
        "opus" => canonical.contains("opus"),
        "sonnet" => canonical.contains("sonnet"),
        "haiku" => canonical.contains("haiku"),
        _ => false,
    }
}

/// Resolve a full model id to a shorter canonical family name. Faithful copy of
/// `tools/skill/src/model_override.rs::canonical_name` (ports
/// `firstPartyNameToCanonical`, `model.ts:217-270`); the
/// `resolveOverriddenModel`/Bedrock-ARN indirection (`getCanonicalName`,
/// `model.ts:279-283`) is a no-op for these substring checks, so it is folded
/// in (identical to the skill copy's note).
fn canonical_name(model: &str) -> String {
    let name = model.to_lowercase();
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

/// Resolve a user/agent-specified model string to a concrete id — the
/// alias-resolution subset of `parseUserSpecifiedModel` (`model.ts:445-505`),
/// EXACTLY as in `tools/skill/src/model_override.rs`. Bare family aliases
/// (`opus` / `sonnet` / `haiku` / `opusplan` / `best`) map to their default id
/// (preserving a trailing `[1m]`, except `best` which `getBestModel` ignores
/// `[1m]` for); every other string passes through with only `[1m]` normalized.
///
/// The alias arms route through the env-aware `get_default_*_model` helpers
/// (matching the TS `getDefaultSonnetModel()` / `getDefaultHaikuModel()` /
/// `getDefaultOpusModel()` calls at `model.ts:459-465`), so the
/// `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` env overrides take effect.
///
/// The ant-model registry, the legacy Opus 4.0/4.1 first-party remap, and the
/// Foundry deployment-id passthrough branches of `parseUserSpecifiedModel` are
/// NOT modeled (unreachable from this seam — see the skill copy's note).
///
/// Public entry point — claude's `getMainLoopModel()` → `parseUserSpecifiedModel`.
/// The composition root calls this to resolve the user/CLI model setting (a bare
/// alias like `opusplan`/`sonnet`, or a full id) to the concrete main-loop wire
/// id BEFORE handing it to a subagent/teammate spawner as the `parent_model`.
/// Without this the `AgentModel::Inherit` default-mode branch returns the raw
/// alias (a bogus wire id); claude passes the RESOLVED `mainLoopModel` as the
/// child's parent. The raw alias is still passed separately as `model_setting`
/// so the plan-mode `opusplan→Opus` swap can fire.
#[must_use]
pub fn resolve_user_specified_model(model_input: &str) -> String {
    parse_user_specified_model(model_input)
}

fn parse_user_specified_model(model_input: &str) -> String {
    let trimmed = model_input.trim();
    let normalized = trimmed.to_lowercase();
    let has_1m_tag = has_1m_context(&normalized);
    let base = strip_1m_suffix(&normalized);
    let suffix = if has_1m_tag { "[1m]" } else { "" };

    match base.as_str() {
        // `opusplan` → Sonnet default (Opus only in plan mode), 1:1 with TS.
        "opusplan" | "sonnet" => format!("{}{suffix}", get_default_sonnet_model()),
        "haiku" => format!("{}{suffix}", get_default_haiku_model()),
        "opus" => format!("{}{suffix}", get_default_opus_model()),
        // `getBestModel` returns the Opus default and ignores any [1m] suffix.
        "best" => get_default_opus_model(),
        _ => {
            // Non-alias: preserve the original case, normalizing only `[1m]`.
            if has_1m_tag {
                format!("{}[1m]", strip_1m_suffix(trimmed))
            } else {
                trimmed.to_string()
            }
        }
    }
}

/// `true` if `model` carries an explicit `[1m]` suffix (case-insensitive).
/// Mirrors `has1mContext` reduced to the substring test (the
/// `CLAUDE_CODE_DISABLE_1M_CONTEXT` gate is not modeled here — `[1m]` passthrough
/// is a no-op for the family-default resolution this seam performs).
fn has_1m_context(model: &str) -> bool {
    model.to_lowercase().contains("[1m]")
}

/// Strip a single trailing `[1m]` (case-insensitive) and trim. Mirrors the TS
/// `replace(/\[1m]$/i, '').trim()`.
fn strip_1m_suffix(s: &str) -> String {
    if s.to_lowercase().ends_with("[1m]") {
        s[..s.len() - 4].trim().to_string()
    } else {
        s.trim().to_string()
    }
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
/// (→ `false` → Explore inherits). The env half (`CLAUDE_CODE_USE_BEDROCK` /
/// `_VERTEX` / `_FOUNDRY`) is checked here, same as `fr()`.
fn session_model_exceeds_explore_cap(
    session_model: &str,
    session_provider_first_party: bool,
) -> bool {
    if !session_provider_first_party || !api_provider_is_first_party() {
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
            .or_else(|_| std::env::var("CLAUDE_CODE_DISABLE_EXPLORE_INHERIT_CAP"))
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

/// Resolve `model` to a concrete wire model string given the parent / main-loop
/// model, the live permission mode, and the RAW user model setting. 1:1 port of
/// `getAgentModel` (`agent.ts:37-95`). See the module docs for the precedence and
/// the deferred nuances.
///
/// `permission_mode` is the live/boot permission mode anchor (Inherit routes
/// through `getRuntimeMainLoopModel`). `model_setting` is the RAW user model
/// setting string (`getUserSpecifiedModelSetting()`, e.g. `"opusplan"` /
/// `"haiku"` / `None`) — NOT the resolved id; without it the Inherit branch
/// returns the parent model unchanged (faithful: a non-opusplan setting never
/// triggers the plan-mode swap).
#[must_use]
pub fn resolve_agent_model(
    model: &AgentModel,
    parent_model: &str,
    permission_mode: PermissionMode,
    model_setting: Option<&str>,
) -> String {
    resolve_agent_model_restricted(
        model,
        parent_model,
        permission_mode,
        model_setting,
        None,
        &mut |_| {},
    )
}

/// [`resolve_agent_model`] with the managed-allowlist subagent gate the binary
/// `ble`/`Qly` applies. When a subagent's EXPLICITLY-requested (or
/// `LINGXI_SUBAGENT_MODEL`-env) model resolves to an id BARRED by the managed
/// allowlist, the request is dropped and the subagent inherits the parent /
/// runtime main-loop model (itself plan-mode-gated), emitting the byte-exact
/// `Subagent model "<m>" is not in the availableModels allowlist; inheriting the
/// parent model instead` warning through `warn` (the caller de-duplicates,
/// mirroring the binary `SN` set).
///
/// An `AgentModel::Inherit` request, or a bare family alias that tier-matches the
/// parent, is NEVER barred — those already resolve to the parent/runtime model.
/// With `restriction == None` (or an inactive one) this is byte-identical to the
/// unrestricted resolution.
#[must_use]
pub fn resolve_agent_model_restricted(
    model: &AgentModel,
    parent_model: &str,
    permission_mode: PermissionMode,
    model_setting: Option<&str>,
    restriction: Option<ModelRestriction<'_>>,
    warn: &mut dyn FnMut(&str),
) -> String {
    // The inherited / runtime main-loop model an explicitly-requested-but-barred
    // subagent (and the Inherit branch) falls back to — plan-mode-gated (binary
    // `i()` = `RF(...)`).
    let inherit = |warn: &mut dyn FnMut(&str)| {
        get_runtime_main_loop_model_restricted(
            permission_mode,
            parent_model,
            false,
            model_setting,
            restriction,
            warn,
        )
    };
    // Emit the byte-exact "Subagent model … inheriting the parent model instead"
    // warning (binary `Qly`), naming the REQUESTED model string.
    let warn_subagent = |warn: &mut dyn FnMut(&str), requested: &str| {
        warn(&format!(
            "Subagent model \"{requested}{}",
            allowlist::warnings::NOT_IN_ALLOWLIST_SUBAGENT
        ));
    };

    // 1. LINGXI_SUBAGENT_MODEL env override (HIGHEST). The TS guard is falsy
    //    for both unset AND empty-string. This branch bypasses the Bedrock prefix
    //    (the TS early-return precedes applyParentRegionPrefix).
    if let Some(v) = std::env::var("LINGXI_SUBAGENT_MODEL")
        .ok()
        .filter(|s| !s.is_empty())
    {
        let resolved = parse_user_specified_model(&v);
        if restriction_bars(restriction, &resolved) {
            warn_subagent(warn, &v);
            return inherit(warn);
        }
        return resolved;
    }

    // Extract Bedrock region prefix from the parent model to inherit for
    // subagents (so subagents use the same cross-region inference profile).
    let parent_region_prefix = get_bedrock_region_prefix(parent_model);

    // Apply the parent region prefix for Bedrock models. `original_spec` is the
    // raw model string before resolution (alias or full id). If the user
    // explicitly specified a full model id that already carries its own region
    // prefix (e.g. "eu.anthropic.…"), we preserve it instead of overwriting with
    // the parent's prefix (prevents silent data-residency violations).
    let apply_parent_region_prefix = |resolved: &str, original_spec: &str| -> String {
        if let Some(prefix) = parent_region_prefix {
            if api_provider_is_bedrock() {
                if get_bedrock_region_prefix(original_spec).is_some() {
                    return resolved.to_owned();
                }
                return apply_bedrock_region_prefix(resolved, prefix);
            }
        }
        resolved.to_owned()
    };

    // 2. toolSpecifiedModel (agent.ts:70-76) — NOT separately ported; the spawn
    //    path maps AgentTool's per-call model into AgentModel::Alias upstream, so
    //    the alias/tier tail below covers it (one intentional deviation).

    match model {
        // 3. Inherit → runtime main-loop resolution (opusplan→Opus / haiku→Sonnet
        //    in plan mode; else the parent model unchanged). Never allowlist-gated
        //    (it already yields the parent/runtime model).
        AgentModel::Inherit => inherit(warn),
        // 4. Explicit / Alias share the same tail: if the bare family alias
        //    matches the parent's tier, inherit the parent's EXACT id; else
        //    resolve + apply the parent region prefix. Real explicit ids are
        //    full `claude-…` strings, which parse_user_specified_model passes
        //    through unchanged (case preserved, only [1m] normalized), so this is
        //    byte-identical to the old verbatim passthrough for them. A resolved
        //    model the managed allowlist BARS falls back to the inherited model.
        AgentModel::Explicit(spec) | AgentModel::Alias(spec) => {
            if alias_matches_parent_tier(spec, parent_model) {
                return parent_model.to_string();
            }
            let resolved = parse_user_specified_model(spec);
            let resolved = apply_parent_region_prefix(&resolved, spec);
            if restriction_bars(restriction, &resolved) {
                warn_subagent(warn, spec);
                return inherit(warn);
            }
            resolved
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- env-var serialization -------------------------------------------
    //
    // LINGXI_SUBAGENT_MODEL / CLAUDE_CODE_USE_BEDROCK / ANTHROPIC_DEFAULT_*
    // mutate process-global env. To avoid cross-test races (cargo runs tests in
    // parallel within a crate), all env-mutating tests share one Mutex and clean
    // up after themselves. (migrations/src/context.rs sets env directly; we add a
    // guard since multiple tests here touch the same vars.)
    use std::sync::Mutex;
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, val);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    const DEFAULT: PermissionMode = PermissionMode::Default;

    #[test]
    fn inherit_resolves_to_parent_model() {
        assert_eq!(
            resolve_agent_model(&AgentModel::Inherit, "claude-opus-4-7", DEFAULT, None),
            "claude-opus-4-7"
        );
    }

    #[test]
    fn explicit_passes_through_verbatim() {
        // Real explicit ids are full `claude-…` strings → parse passes them
        // through unchanged.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Explicit("claude-sonnet-4-6".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn family_alias_of_different_tier_resolves_to_default_id() {
        // parent is opus; agent asks for haiku/sonnet → family default concrete id.
        // The `sonnet` default is provider-aware (#18) so pin firstParty under the
        // ENV_LOCK to assert the 1P id deterministically.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("haiku".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "claude-haiku-4-5"
        );
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn family_alias_matching_parent_tier_inherits_exact_parent() {
        // parent IS opus → "opus" inherits the parent's exact id, not a default.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opus".to_string()),
                "claude-opus-4-9-future",
                DEFAULT,
                None,
            ),
            "claude-opus-4-9-future"
        );
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "claude-sonnet-4-6",
                DEFAULT,
                None,
            ),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn family_alias_is_case_insensitive() {
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("Haiku".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn non_family_alias_passes_through() {
        // A custom alias the user may have mapped via routing.aliases.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("my-fast-model".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "my-fast-model"
        );
    }

    #[test]
    fn family_alias_with_non_anthropic_parent_resolves_to_default_id() {
        // A 3P / non-Anthropic parent MODEL is not any Claude tier, so a family
        // alias resolves to the family default (NOT the parent). Pin firstParty
        // under the ENV_LOCK since the `sonnet` default is provider-aware (#18);
        // the parent model id (gemini/…) is unrelated to the API provider env.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opus".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "claude-opus-4-8"
        );
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "gemini/gemini-2.0-flash",
                DEFAULT,
                None,
            ),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn cross_tier_family_alias_resolves_to_that_familys_default() {
        // parent is sonnet, agent asks for opus (different tier) → opus default id.
        // The opus default is provider-aware (#2), so pin firstParty deterministically.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opus".to_string()),
                "claude-sonnet-4-6",
                DEFAULT,
                None,
            ),
            "claude-opus-4-8"
        );
    }

    // ---- G10: new edge cases ----------------------------------------------

    #[test]
    fn subagent_model_env_override_wins_over_everything() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("LINGXI_SUBAGENT_MODEL", "haiku");
        // Override beats Inherit, Alias, and Explicit alike — resolved via
        // parse_user_specified_model.
        assert_eq!(
            resolve_agent_model(&AgentModel::Inherit, "claude-opus-4-7", DEFAULT, None),
            "claude-haiku-4-5"
        );
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "claude-haiku-4-5"
        );
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Explicit("claude-sonnet-4-6".to_string()),
                "claude-opus-4-7",
                DEFAULT,
                None,
            ),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn subagent_model_env_override_empty_string_is_ignored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("LINGXI_SUBAGENT_MODEL", "");
        // Empty-string is falsy in the TS guard → not honored; Inherit falls
        // through to the parent.
        assert_eq!(
            resolve_agent_model(&AgentModel::Inherit, "claude-opus-4-7", DEFAULT, None),
            "claude-opus-4-7"
        );
    }

    #[test]
    fn inherit_opusplan_plan_mode_resolves_to_opus() {
        // opusplan in plan mode resolves to the opus default, which is
        // provider-aware (#2) → pin firstParty deterministically.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Inherit,
                "claude-sonnet-4-6",
                PermissionMode::Plan,
                Some("opusplan"),
            ),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn inherit_opusplan_default_mode_returns_parent_unchanged() {
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Inherit,
                "claude-sonnet-4-6",
                PermissionMode::Default,
                Some("opusplan"),
            ),
            "claude-sonnet-4-6"
        );
    }

    #[test]
    fn inherit_haiku_plan_mode_resolves_to_sonnet() {
        // Resolves to the Sonnet default, which is provider-aware (#18) → pin
        // firstParty under the ENV_LOCK to assert the 1P id deterministically.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Inherit,
                "claude-opus-4-7",
                PermissionMode::Plan,
                Some("haiku"),
            ),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn inherit_no_setting_plan_mode_returns_parent_unchanged() {
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Inherit,
                "claude-opus-4-7",
                PermissionMode::Plan,
                None
            ),
            "claude-opus-4-7"
        );
    }

    // ---- G10: Bedrock cross-region prefix inheritance ----------------------

    #[test]
    fn bedrock_family_default_is_not_a_foundation_model_so_prefix_is_noop() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        // parent has a us. prefix; on Bedrock (non-firstParty) the `sonnet` alias
        // resolves to the 3P Sonnet default (claude-sonnet-4-5-20250929, #18),
        // which is NOT a foundation (anthropic.*) id, so apply_bedrock_region_prefix
        // is a no-op.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "us.anthropic.claude-opus-4-6-v1:0",
                DEFAULT,
                None,
            ),
            "claude-sonnet-4-5-20250929"
        );
    }

    #[test]
    fn bedrock_foundation_model_inherits_parent_prefix() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        // An explicit foundation id (anthropic.*) gets the parent's us. prefix.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Explicit("anthropic.claude-sonnet-4-6-v1:0".to_string()),
                "us.anthropic.claude-opus-4-6-v1:0",
                DEFAULT,
                None,
            ),
            "us.anthropic.claude-sonnet-4-6-v1:0"
        );
    }

    #[test]
    fn bedrock_original_spec_with_own_prefix_is_preserved() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        // The explicit spec already pins eu. → the parent's us. prefix does NOT
        // overwrite it.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Explicit("eu.anthropic.claude-sonnet-4-6-v1:0".to_string()),
                "us.anthropic.claude-opus-4-6-v1:0",
                DEFAULT,
                None,
            ),
            "eu.anthropic.claude-sonnet-4-6-v1:0"
        );
    }

    #[test]
    fn bedrock_prefix_not_applied_when_not_bedrock_provider() {
        // No CLAUDE_CODE_USE_BEDROCK → api_provider_is_bedrock() is false → the
        // foundation id is returned unchanged even though the parent has a us.
        // prefix. (Guarded by the lock to avoid a concurrent test setting the var.)
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard {
            key: "CLAUDE_CODE_USE_BEDROCK",
            prev: std::env::var("CLAUDE_CODE_USE_BEDROCK").ok(),
        };
        std::env::remove_var("CLAUDE_CODE_USE_BEDROCK");
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Explicit("anthropic.claude-sonnet-4-6-v1:0".to_string()),
                "us.anthropic.claude-opus-4-6-v1:0",
                DEFAULT,
                None,
            ),
            "anthropic.claude-sonnet-4-6-v1:0"
        );
    }

    #[test]
    fn alias_matches_parent_tier_via_canonical_inherits_exact_parent() {
        // parent is a Bedrock-prefixed opus id; canonical_name strips the prefix
        // so Alias("opus") tier-matches and inherits the EXACT parent id.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opus".to_string()),
                "us.anthropic.claude-opus-4-6-v1:0",
                DEFAULT,
                None,
            ),
            "us.anthropic.claude-opus-4-6-v1:0"
        );
    }

    #[test]
    fn non_bare_aliases_do_not_tier_match() {
        // opus[1m] / best / opusplan are NOT bare family aliases → default arm
        // false, so they do NOT inherit the parent even when same family.
        assert!(!alias_matches_parent_tier("opus[1m]", "claude-opus-4-7"));
        assert!(!alias_matches_parent_tier("best", "claude-opus-4-7"));
        assert!(!alias_matches_parent_tier("opusplan", "claude-opus-4-7"));
        assert!(alias_matches_parent_tier("opus", "claude-opus-4-7"));
    }

    #[test]
    fn default_opus_model_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("ANTHROPIC_DEFAULT_OPUS_MODEL", "custom-opus-id");
        assert_eq!(get_default_opus_model(), "custom-opus-id");
    }

    // ---- #18: 3P-provider Sonnet family default (Bedrock/Vertex/Foundry) ------
    //
    // claude-code's getDefaultSonnetModel() (model.ts:118-128) returns
    // getModelStrings().sonnet45 (canonical claude-sonnet-4-5-20250929) for
    // non-firstParty providers, and sonnet46 (claude-sonnet-4-6) for firstParty.
    // getDefaultOpusModel/getDefaultHaikuModel do NOT differ by provider today.
    //
    // Each test scopes ALL THREE provider env vars under the shared ENV_LOCK so a
    // concurrent test setting CLAUDE_CODE_USE_* cannot leak in.

    /// Clear all three provider env vars (restored on Drop), pinning firstParty.
    fn clear_provider_env() -> [EnvGuard; 3] {
        let mk = |k: &'static str| {
            let g = EnvGuard {
                key: k,
                prev: std::env::var(k).ok(),
            };
            std::env::remove_var(k);
            g
        };
        [
            mk("CLAUDE_CODE_USE_BEDROCK"),
            mk("CLAUDE_CODE_USE_VERTEX"),
            mk("CLAUDE_CODE_USE_FOUNDRY"),
        ]
    }

    #[test]
    fn sonnet_default_is_5_on_first_party() {
        // 2.1.198 alias table: sonnet.default = claude-sonnet-5.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        assert!(api_provider_is_first_party());
        assert_eq!(get_default_sonnet_model(), "claude-sonnet-5");
        // The bare `sonnet` alias resolves through the same default on firstParty.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "claude-sonnet-5"
        );
    }

    #[test]
    fn sonnet_default_is_4_5_on_bedrock() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        assert!(!api_provider_is_first_party());
        assert_eq!(get_default_sonnet_model(), "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn sonnet_default_is_4_5_on_vertex() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _v = EnvGuard::set("CLAUDE_CODE_USE_VERTEX", "1");
        assert!(!api_provider_is_first_party());
        assert_eq!(get_default_sonnet_model(), "claude-sonnet-4-5-20250929");
        // The bare `sonnet` alias resolves to the 3P default on Vertex. Parent is
        // non-Anthropic so no tier-match early-return can shadow it.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "claude-sonnet-4-5-20250929"
        );
        // `opusplan` also resolves to the Sonnet default outside plan mode, so it
        // too gets the 3P id.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opusplan".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "claude-sonnet-4-5-20250929"
        );
    }

    #[test]
    fn sonnet_default_is_4_5_on_foundry() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _f = EnvGuard::set("CLAUDE_CODE_USE_FOUNDRY", "1");
        assert!(!api_provider_is_first_party());
        assert_eq!(get_default_sonnet_model(), "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn sonnet_env_override_wins_over_provider_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
        // ANTHROPIC_DEFAULT_SONNET_MODEL (non-empty) beats the 3P provider branch.
        let _o = EnvGuard::set("ANTHROPIC_DEFAULT_SONNET_MODEL", "custom-sonnet-id");
        assert_eq!(get_default_sonnet_model(), "custom-sonnet-id");
    }

    #[test]
    fn non_truthy_provider_env_stays_first_party() {
        // A non-allowlisted value ("0") is NOT truthy under the strict allowlist,
        // so the provider stays firstParty and Sonnet stays 4.6 — matching the TS
        // isEnvTruthy semantics.
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "0");
        assert!(api_provider_is_first_party());
        assert_eq!(get_default_sonnet_model(), "claude-sonnet-5");
    }

    #[test]
    fn opus_differs_by_provider_haiku_does_not() {
        // 2.1.207 alias table: opus.default = claude-opus-4-8 with
        // per_provider{bedrock:4-8, vertex:4-8, foundry:4-6, …}. Only Foundry
        // diverges now (Bedrock/Vertex joined the 4-8 default). Haiku has no
        // provider branch (same id on all platforms).
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        // firstParty → 4-8
        assert_eq!(get_default_opus_model(), "claude-opus-4-8");
        let haiku_fp = get_default_haiku_model();
        // Bedrock → 4-8 (2.1.207 change)
        {
            let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
            assert_eq!(get_default_opus_model(), "claude-opus-4-8");
            assert_eq!(get_default_haiku_model(), haiku_fp);
        }
        // Vertex → 4-8 (2.1.207 change)
        {
            let _v = EnvGuard::set("CLAUDE_CODE_USE_VERTEX", "1");
            assert_eq!(get_default_opus_model(), "claude-opus-4-8");
        }
        // Foundry → 4-6 (still diverges)
        {
            let _f = EnvGuard::set("CLAUDE_CODE_USE_FOUNDRY", "1");
            assert_eq!(get_default_opus_model(), "claude-opus-4-6");
        }
        // Precedence: Bedrock outranks Foundry → 4-8 even with both set.
        {
            let _f = EnvGuard::set("CLAUDE_CODE_USE_FOUNDRY", "1");
            let _b = EnvGuard::set("CLAUDE_CODE_USE_BEDROCK", "1");
            assert_eq!(get_default_opus_model(), "claude-opus-4-8");
        }
        // ANTHROPIC_DEFAULT_OPUS_MODEL override wins even on Foundry.
        {
            let _f = EnvGuard::set("CLAUDE_CODE_USE_FOUNDRY", "1");
            let _o = EnvGuard::set("ANTHROPIC_DEFAULT_OPUS_MODEL", "custom-opus-id");
            assert_eq!(get_default_opus_model(), "custom-opus-id");
        }
    }

    // ---- M2 Part B: registry pins (per-provider alias-table identity) -------
    //
    // Verified against the REAL 2.1.207 binary registry (extracted 2026-07-14):
    // - alias table `sonnet.per_provider = {bedrock/vertex/foundry/mantle →
    //   "claude-sonnet-4-5", anthropic_aws/gateway → "claude-sonnet-4-6"}`,
    //   default "claude-sonnet-5".
    // - alias table `opus.per_provider = {bedrock/vertex/mantle/anthropic_aws →
    //   "claude-opus-4-8", foundry → "claude-opus-4-6", gateway →
    //   "claude-opus-4-7"}`, default "claude-opus-4-8". (2.1.207 moved
    //   Bedrock/Vertex/Claude-on-AWS onto the 4-8 default; only Foundry stays
    //   4-6.)
    // - sonnet-5 `provider_ids.anthropic_aws = "claude-sonnet-5"` (same string
    //   as first_party — the id does not diverge for anthropicAws).
    //
    // LingXi's provider detection is 2-way (firstParty vs the env-detected
    // Bedrock/Vertex/Foundry), so the anthropic_aws / mantle / gateway arms
    // have no runtime representation — the ids they'd resolve to are pinned
    // here so a future anthropicAws-aware host wires the RIGHT values (and any
    // upstream alias-table change shows up as a deliberate test edit).

    #[test]
    fn registry_2_1_207_pins_match_binary() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        // Defaults (firstParty arm) — shared with the anthropic_aws sonnet-5
        // provider id, which is byte-identical to first_party.
        assert_eq!(family_default_id("sonnet"), Some("claude-sonnet-5"));
        assert_eq!(family_default_id("opus"), Some("claude-opus-4-8"));
        assert_eq!(family_default_id("haiku"), Some("claude-haiku-4-5"));
        // Foundry arm — the only Opus per_provider divergence in 2.1.207.
        assert_eq!(OPUS_FOUNDRY_DEFAULT_ID, "claude-opus-4-6");
        // Sonnet 3P arm (bedrock/vertex/foundry per_provider) unchanged.
        assert_eq!(SONNET_3P_DEFAULT_ID, "claude-sonnet-4-5-20250929");
    }

    #[test]
    fn registry_anthropic_aws_gateway_alias_targets_pass_through() {
        // The 2.1.207 per_provider alias targets (sonnet anthropic_aws/gateway
        // → claude-sonnet-4-6, opus gateway → claude-opus-4-7) are real catalog
        // ids: an explicit request for either must pass through verbatim so an
        // anthropicAws/gateway-configured profile can route them unmodified.
        for id in ["claude-sonnet-4-6", "claude-opus-4-7"] {
            assert_eq!(
                resolve_agent_model(
                    &AgentModel::Explicit(id.to_string()),
                    "claude-opus-4-8",
                    DEFAULT,
                    None,
                ),
                id
            );
        }
    }

    // ---- Tier3#14: alias arms honor ANTHROPIC_DEFAULT_*_MODEL env overrides --
    //
    // claude-code's parseUserSpecifiedModel routes its alias arms through
    // getDefaultSonnetModel() / getDefaultHaikuModel() / getDefaultOpusModel()
    // (model.ts:459-465), so the ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL env
    // overrides win for the corresponding family alias. The full resolve path
    // (resolve_agent_model → parse_user_specified_model) must reflect that.
    //
    // Parent is non-Anthropic ("openai/gpt-4o") so no tier-match early-return
    // can shadow the alias resolution.

    #[test]
    fn opus_alias_honors_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("ANTHROPIC_DEFAULT_OPUS_MODEL", "custom-opus-id");
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opus".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "custom-opus-id"
        );
        // The [1m] suffix is preserved on top of the env override.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opus[1m]".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "custom-opus-id[1m]"
        );
    }

    #[test]
    fn sonnet_alias_honors_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("ANTHROPIC_DEFAULT_SONNET_MODEL", "custom-sonnet-id");
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("sonnet".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "custom-sonnet-id"
        );
        // `opusplan` resolves to the Sonnet default outside plan mode, so it too
        // honors ANTHROPIC_DEFAULT_SONNET_MODEL.
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("opusplan".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "custom-sonnet-id"
        );
    }

    #[test]
    fn haiku_alias_honors_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("ANTHROPIC_DEFAULT_HAIKU_MODEL", "custom-haiku-id");
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("haiku".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "custom-haiku-id"
        );
    }

    #[test]
    fn best_alias_honors_opus_env_override() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = EnvGuard::set("ANTHROPIC_DEFAULT_OPUS_MODEL", "custom-opus-id");
        // `best` → getBestModel → Opus default (ignoring any [1m] suffix).
        assert_eq!(
            resolve_agent_model(
                &AgentModel::Alias("best".to_string()),
                "openai/gpt-4o",
                DEFAULT,
                None,
            ),
            "custom-opus-id"
        );
    }

    // ── GAe/obm/dPn (2.1.198): built-in Explore model from the session model ──

    /// A built-in Explore stand-in matching the fields `GAe` consults.
    fn builtin_explore_def() -> AgentDefinition {
        let mut def = crate::builtins::builtin_agent_definitions()
            .into_iter()
            .find(|d| d.agent_type == "Explore")
            .expect("Explore is a built-in");
        // The 2.1.198 frontmatter is `model:"inherit"` (qme); assert it here so
        // the GAe tests below exercise the real definition.
        assert!(matches!(def.model, AgentModel::Inherit));
        def.model = AgentModel::Inherit;
        def
    }

    #[test]
    fn explore_on_fable_class_session_caps_at_opus() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let def = builtin_explore_def();
        // fable/mythos-class session models name none of haiku/sonnet/opus →
        // obm true → the "opus" alias (Yyl).
        for session in [
            "claude-fable-5-1",
            "claude-mythos-5-1-20260901",
            "CLAUDE-FABLE-5[1m]",
        ] {
            assert!(
                matches!(
                    resolve_builtin_explore_model(&def, session, true),
                    AgentModel::Alias(ref a) if a == "opus"
                ),
                "{session} → opus cap"
            );
        }
    }

    /// 2.1.266 `yX`'s kill-switch, ahead of the cap test: a deployment can let
    /// Explore run on the full session model.
    #[test]
    fn explore_inherit_cap_kill_switch_restores_plain_inherit() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let def = builtin_explore_def();
        // Premise: this session WOULD be capped without the switch, so a pass
        // below cannot come from the session model being under the cap anyway.
        assert!(
            matches!(
                resolve_builtin_explore_model(&def, "claude-fable-5-1", true),
                AgentModel::Alias(ref a) if a == "opus"
            ),
            "premise: a fable-class session is capped",
        );
        std::env::set_var("LINGXI_DISABLE_EXPLORE_INHERIT_CAP", "1");
        let got = resolve_builtin_explore_model(&def, "claude-fable-5-1", true);
        std::env::remove_var("LINGXI_DISABLE_EXPLORE_INHERIT_CAP");
        assert!(
            matches!(got, AgentModel::Inherit),
            "the kill-switch returns plain `inherit`, got {got:?}",
        );
    }

    #[test]
    fn explore_on_claude_family_session_inherits() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let def = builtin_explore_def();
        // dPn is a case-insensitive substring test over ["haiku","sonnet","opus"].
        for session in [
            "claude-sonnet-5",
            "claude-opus-4-8-20260115",
            "claude-haiku-4-5",
            "CLAUDE-OPUS-4-6",
        ] {
            assert!(
                matches!(
                    resolve_builtin_explore_model(&def, session, true),
                    AgentModel::Inherit
                ),
                "{session} → inherit"
            );
        }
    }

    #[test]
    fn explore_on_non_first_party_env_provider_inherits() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let def = builtin_explore_def();
        // fr() !== "firstParty" (Bedrock/Vertex/Foundry) → obm false → inherit,
        // even for a fable-class session model.
        for var in [
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            let _p = EnvGuard::set(var, "1");
            assert!(
                matches!(
                    resolve_builtin_explore_model(&def, "claude-fable-5-1", true),
                    AgentModel::Inherit
                ),
                "{var} → inherit"
            );
        }
    }

    #[test]
    fn explore_on_non_anthropic_profile_inherits() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let def = builtin_explore_def();
        // LingXi multi-provider: a session routed to a non-Anthropic provider
        // profile (OpenAI/Gemini/…) behaves like the non-firstParty branch.
        assert!(matches!(
            resolve_builtin_explore_model(&def, "gpt-4o", false),
            AgentModel::Inherit
        ));
    }

    #[test]
    fn non_explore_and_non_builtin_defs_keep_their_model() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        // Non-Explore built-in: model untouched (GAe early-return on agentType).
        let plan = crate::builtins::builtin_agent_definitions()
            .into_iter()
            .find(|d| d.agent_type == "Plan")
            .unwrap();
        assert!(matches!(
            resolve_builtin_explore_model(&plan, "claude-fable-5-1", true),
            AgentModel::Inherit
        ));
        let statusline = crate::builtins::builtin_agent_definitions()
            .into_iter()
            .find(|d| d.agent_type == "statusline-setup")
            .unwrap();
        assert!(matches!(
            resolve_builtin_explore_model(&statusline, "claude-fable-5-1", true),
            AgentModel::Alias(ref a) if a == "sonnet"
        ));
        // A USER-DEFINED agent literally named "Explore": source != built-in →
        // untouched (GAe early-return on source).
        let mut user_explore = builtin_explore_def();
        user_explore.source = AgentSource::Settings(lingxi_core::types::SettingsScope::User);
        user_explore.model = AgentModel::Alias("sonnet".to_string());
        assert!(matches!(
            resolve_builtin_explore_model(&user_explore, "claude-fable-5-1", true),
            AgentModel::Alias(ref a) if a == "sonnet"
        ));
    }

    // ── H-BIN-08: managed availableModels restriction (binary RF / ble/Qly) ──
    //
    // These exercise the boot-wired managed-allowlist gate. All pin firstParty
    // (clear_provider_env) so the provider-aware Opus/Sonnet defaults are
    // deterministic, and clear LINGXI_SUBAGENT_MODEL so the env branch is inert.

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
            prev: std::env::var("LINGXI_SUBAGENT_MODEL").ok(),
        };
        std::env::remove_var("LINGXI_SUBAGENT_MODEL");
        g
    }

    #[test]
    fn subagent_disallowed_model_inherits_parent_with_exact_warning() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        // Allowlist permits only opus; the request resolves to Sonnet (barred).
        let enf = active(&["opus"]);
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        // Sonnet IS allowed → the request resolves normally with no warning.
        let enf = active(&["opus", "sonnet"]);
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = EnvGuard::set("LINGXI_SUBAGENT_MODEL", "sonnet");
        let enf = active(&["opus"]);
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        let enf = ModelEnforcement::Inactive;
        let cat = catalog(&["claude-opus-4-8", "claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        // Identical to the unrestricted resolution: Alias("sonnet") → default id.
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        // The opus upgrade (claude-opus-4-8) is barred; 4-6 is permitted.
        let enf = active(&["opus-4-6"]);
        let cat = catalog(&["claude-opus-4-6", "claude-opus-4-8"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        // No opus is permitted at all → the resting model (opusplan → Sonnet).
        let enf = active(&["sonnet"]);
        let cat = catalog(&["claude-sonnet-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
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
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        // No sonnet is permitted → the resting model for `haiku` = Haiku default.
        let enf = active(&["haiku"]);
        let cat = catalog(&["claude-haiku-4-5"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
        let _g = clear_provider_env();
        let _s = clear_subagent_env();
        // The opus upgrade IS permitted → no substitution, no warning.
        let enf = active(&["opus"]);
        let cat = catalog(&["claude-opus-4-8"]);
        let restriction = ModelRestriction {
            enforcement: &enf,
            catalog: &cat,
        };
        let mut warns: Vec<String> = Vec::new();
        let out = resolve_agent_model_restricted(
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
