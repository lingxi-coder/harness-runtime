//! Source-aware selection for Native managed settings.
//!
//! This module keeps the Native policy sources separate until the selector
//! chooses them. It never rebuilds a policy object through `serde_json::Value`:
//! exact source projections, including UTF-16 strings, remain attached to
//! their source metadata and admission result.
//!
//! Native policy normalization is separate from ordinary `SettingsJson`
//! loading. The selector uses the parser's source-preserving normalized
//! projection and keeps any current typed `SettingsJson` view optional.

use super::native_policy_js_semantics::NativePolicyJsSemanticsRegistry;
use super::native_policy_parser::{
    parse_policy_projection, NativePolicyHelperDiagnostic, NativePolicyParseDiagnostic,
    NativePolicyParseStatus, NativePolicyStartupFatalDiagnostic,
};
use super::native_policy_regex::NativePolicyRegexValidationRow;
use super::native_policy_static_defaults::{
    native_ftr_settings_error, native_qn_log_messages_from_source,
    native_static_settings_diagnostic, validate_native_helper_managed_settings,
    validate_native_static_default_settings, NativeStaticSettingsStatus,
};
use crate::settings::SettingsJson;
use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::path::PathBuf;

/// Return the source-exact warning messages emitted by Native `qn()` for one
/// validated settings value. Rows stay as UTF-16 projections until the host's
/// final logging sink.
#[must_use]
pub fn native_policy_qn_log_messages(
    raw: &Utf16JsonProjection,
    warnings: &[Utf16JsonProjection],
    source_field: &str,
) -> Option<Vec<Utf16JsonProjection>> {
    native_qn_log_messages_from_source(raw, warnings, source_field)
}

/// Exact source strings a caller must submit to the pinned Bun semantic worker
/// before this policy projection can be admitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativePolicySemanticInputs {
    pub regex_patterns: Vec<Vec<u16>>,
    pub url_inputs: Vec<Vec<u16>>,
    pub display_inputs: Vec<Vec<u16>>,
}

impl NativePolicySemanticInputs {
    pub(super) fn push_unique(values: &mut Vec<Vec<u16>>, value: Vec<u16>) {
        if !values.iter().any(|existing| existing == &value) {
            values.push(value);
        }
    }

    fn extend(&mut self, other: Self) {
        for value in other.regex_patterns {
            Self::push_unique(&mut self.regex_patterns, value);
        }
        for value in other.url_inputs {
            Self::push_unique(&mut self.url_inputs, value);
        }
        for value in other.display_inputs {
            Self::push_unique(&mut self.display_inputs, value);
        }
    }
}

/// Results produced by the pinned Bun 1.4.3 policy worker. A caller must pass
/// the real worker results; there is intentionally no default/empty evidence
/// constructor for production call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicySemanticEvidence {
    pub regex_rows: Vec<NativePolicyRegexValidationRow>,
    pub js: NativePolicyJsSemanticsRegistry,
}

impl NativePolicySemanticEvidence {
    pub fn covers(&self, inputs: &NativePolicySemanticInputs) -> bool {
        inputs.regex_patterns.iter().all(|pattern| {
            self.regex_rows
                .iter()
                .any(|row| row.pattern_units.as_slice() == pattern.as_slice())
        }) && inputs
            .url_inputs
            .iter()
            .all(|input| self.js.url(input).is_some())
            && inputs
                .display_inputs
                .iter()
                .all(|input| self.js.display(input).is_some())
    }
}

/// Collect source-dependent operations that the fixed Native At/Due paths can
/// reach. The first pass uses no JS results; a later pass may provide those
/// results so conditional consumers (currently OAuth metadata URLs) can be
/// discovered without scanning unrelated keys.
pub fn collect_native_policy_semantic_inputs(
    source: &Utf16JsonProjection,
) -> NativePolicySemanticInputs {
    collect_native_policy_semantic_inputs_with_js(source, None)
}

/// Re-run source reachability after the worker has supplied any unconditional
/// URL/display rows. This exposes nested operations only when their enclosing
/// source transform reaches them.
pub fn collect_native_policy_semantic_inputs_with_js(
    source: &Utf16JsonProjection,
    js: Option<&NativePolicyJsSemanticsRegistry>,
) -> NativePolicySemanticInputs {
    let mut collected = NativePolicySemanticInputs::default();

    // `At` walks hooks, extra marketplaces, then managed MCP servers. Their
    // dynamic display/URL work is grouped this way before the Due schema pass.
    if let Ok(hooks) = source.subprojection("/hooks") {
        let hook_inputs =
            super::native_policy_hook_settings::collect_native_hook_semantic_inputs(&hooks);
        for units in hook_inputs.url_inputs {
            NativePolicySemanticInputs::push_unique(&mut collected.url_inputs, units);
        }
        for units in hook_inputs.display_inputs {
            NativePolicySemanticInputs::push_unique(&mut collected.display_inputs, units);
        }
    }
    super::native_extra_known_marketplaces::collect_native_extra_marketplace_url_inputs(
        source,
        &mut collected.url_inputs,
    );
    if let Ok(servers) = source.subprojection("/managedMcpServers") {
        super::native_managed_mcp_servers::collect_native_managed_mcp_semantic_inputs(
            &servers,
            js,
            &mut collected.url_inputs,
        );
    }

    // The remaining semantic consumers are reached by `Due` in schema order:
    // marketplace URL/RegExp validation precedes the sandbox credential RegExp
    // refinements. The latter collector follows the exact `mask`/schema gates.
    super::native_policy_marketplaces::collect_native_policy_marketplace_semantic_inputs(
        source,
        &mut collected.regex_patterns,
        &mut collected.url_inputs,
    );
    let sandbox_patterns =
        super::native_policy_strict_settings::collect_native_regex_patterns(source);
    for pattern in sandbox_patterns {
        NativePolicySemanticInputs::push_unique(&mut collected.regex_patterns, pattern);
    }
    collected
}

pub fn collect_native_policy_static_fallback_semantic_inputs(
    selection: &NativePolicySelection,
) -> NativePolicySemanticInputs {
    collect_native_policy_static_fallback_semantic_inputs_with_js(selection, None)
}

pub fn collect_native_policy_static_fallback_semantic_inputs_with_js(
    selection: &NativePolicySelection,
    js: Option<&NativePolicyJsSemanticsRegistry>,
) -> NativePolicySemanticInputs {
    let mut collected = NativePolicySemanticInputs::default();
    if let Some(plan) = selection.helper_plan.as_ref() {
        for candidate in &plan.static_fallback_candidates {
            collected.extend(collect_native_policy_semantic_inputs_with_js(
                &candidate.settings,
                js,
            ));
        }
    }
    collected
}

/// Native policy source identity. These identities remain distinct from the
/// generic settings tracer's `Managed` source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicySource {
    /// Authenticated remote managed-settings slot. No remote producer is
    /// connected by this module.
    Remote,
    /// macOS managed-preferences plist or Windows policy registry candidate.
    Plist,
    /// Local `managed-settings.json` and `managed-settings.d/*.json` source.
    ManagedFile,
    /// Settings supplied by an embedding parent process.
    Parent,
    /// Native host/registry fallback used only without admin or parent policy.
    Host,
}

/// The Native reader state attached to a source document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicyLoadState {
    /// A source was not present.
    Absent,
    /// The source produced syntactically valid JSON.
    Loaded,
    /// The source existed but could not be read or decoded.
    DidNotLoad,
}

/// Native host fallback subtype. This is metadata, not a settings document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicyHostKind {
    /// Windows HKCU registry fallback. Absent on macOS.
    RegistryFallback,
    /// An explicit local host settings fallback.
    HostSettingsFallback,
}

/// Host platform used by Native's per-OS policy-helper chain.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NativePolicyPlatform {
    /// macOS policy source; chain contains only `macos`.
    MacOS,
    /// Linux policy source; chain contains only `linux`.
    Linux,
    /// Windows policy source; chain contains only `windows`.
    Windows,
    /// WSL policy source; chain checks `wsl` before `linux`.
    Wsl,
    /// Unknown platform; no per-OS entry is selected.
    #[default]
    Unknown,
}

impl NativePolicyPlatform {
    fn native_key(self) -> Option<&'static str> {
        match self {
            Self::MacOS => Some("macos"),
            Self::Linux => Some("linux"),
            Self::Windows => Some("windows"),
            Self::Wsl => Some("wsl"),
            Self::Unknown => None,
        }
    }

    fn native_chain(self) -> Vec<Self> {
        match self {
            Self::MacOS | Self::Linux | Self::Windows => vec![self],
            Self::Wsl => vec![Self::Wsl, Self::Linux],
            Self::Unknown => Vec::new(),
        }
    }

    fn ordered_policy_keys(self) -> Vec<&'static str> {
        self.native_chain()
            .into_iter()
            .filter_map(Self::native_key)
            .collect()
    }
}

/// Native per-OS helper entry selected from the winning admin source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyHelperEntry {
    /// OS key chosen by Native's platform chain.
    pub entry_key: String,
    /// Normalized entry fields excluding `defaultSettings`.
    pub config: Utf16JsonProjection,
    /// True for a per-OS entry; false for the singular `policyHelper` fallback.
    pub from_per_os: bool,
    /// Effective `onFailure` value. Local admin helpers default to refusal;
    /// remote-only defaults are outside this selector's input scope.
    pub on_failure: NativePolicyHelperOnFailure,
    /// Effective timeout after Native's default and cap.
    pub timeout_ms: u32,
    /// Effective retry count after Native's default and cap.
    pub retries: u8,
    /// Effective refresh interval; zero disables background refresh.
    pub refresh_interval_ms: u32,
    /// Effective helper output composition mode.
    pub output_behavior: NativePolicyHelperOutputBehavior,
}

/// Whether a local helper failure refuses startup when no static fallback
/// applies. Native file and MDM helpers default to `Refuse`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicyHelperOnFailure {
    /// Refuse the start when helper execution fails and no fallback applies.
    Refuse,
    /// Continue without helper output when helper execution fails.
    Continue,
}

/// How valid helper-managed settings compose with the selected admin tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicyHelperOutputBehavior {
    /// Replace the selected admin tier's ordinary settings.
    Replace,
    /// Merge helper settings above the selected admin tier.
    Merge,
}

/// Which source branch Native selected after checking a per-OS helper, static
/// fallback, and singular helper in that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicyHelperSelectedKind {
    /// A helper with a path or inline script exists on the platform chain.
    PerOsHelper,
    /// No per-OS helper exists, so an eligible static payload wins.
    StaticFallback,
    /// No static payload wins, so the singular helper is selected.
    SingularHelper,
    /// The source has no configuration usable by the current platform.
    NoEntry,
}

/// Completeness of the selected source's helper-configuration parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyHelperPlanValidation {
    /// Every present helper field was normalized by the current parser.
    Validated,
    /// A present helper subfield still has source-specific validation to port.
    Unresolved { fields: Vec<String> },
}

/// Validation state for one static helper fallback payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyStaticFallbackValidation {
    /// Full Native `I5e`, `At`, and `Due` admission succeeded.
    Validated,
    /// This parser has not yet represented a source-relevant validator.
    Unresolved { fields: Vec<String> },
    /// A source-backed validator rejected the payload. Its exact `xi()` text
    /// remains on the candidate as a UTF-16 projection.
    Rejected,
}

/// Native helper stdout fields recognized by `Hi()`.
///
/// The full source envelope is retained. The Host stores `claude_md` and
/// `append_system_prompt` as helper state; this Core API applies only the
/// validated `managed_settings` subtree to the policy selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyHelperOutput {
    /// Exact full helper stdout projection.
    pub raw: Utf16JsonProjection,
    /// Exact `managedSettings` object, if present.
    pub managed_settings: Option<Utf16JsonProjection>,
    /// Exact `claudeMd` string projection, if present.
    pub claude_md: Option<Utf16JsonProjection>,
    /// Exact `appendSystemPrompt` string projection, if present.
    pub append_system_prompt: Option<Utf16JsonProjection>,
}

/// Invalid Native helper stdout envelope shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyHelperOutputError {
    /// Native `Zi()` maps JSON primitives (and a failed JSON parse) to this
    /// exact source message before its Zod envelope validator is reached.
    NonObjectRoot { diagnostic: Utf16JsonProjection },
    /// The parsed value reached Native `Hi().safeParse` and failed its exact
    /// envelope schema. `diagnostic` is the Zod error message string, not a
    /// Rust-generated approximation.
    InvalidEnvelope { diagnostic: Utf16JsonProjection },
    /// The exact source member could not be projected from the valid JSON
    /// object. This is an internal source-projection failure, not a Native
    /// validation diagnostic.
    ProjectionUnavailable { field: &'static str },
}

/// Outcome from parsing and applying helper-managed settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyHelperApplyStatus {
    /// The managed settings passed the implemented field parsers and were
    /// applied to a fresh composition of the selected source tiers.
    Applied,
    /// The valid envelope omitted `managedSettings`; there is no policy
    /// subtree to compose, while the helper's prompt fields remain available.
    NoManagedSettings,
    /// The managed settings failed structural JSON/object admission.
    Rejected {
        /// Parser classification detail for logs and tests.
        reason: String,
        /// Exact Native error text when its validator message is source-bound.
        diagnostic: Option<Utf16JsonProjection>,
    },
    /// Present Native policy fields still require a parser before application.
    Unresolved { fields: Vec<String> },
    /// Native source diagnostics require startup refusal before helper output
    /// can be used.
    StartupFatal {
        /// Source-bound diagnostics, including exact Native message
        /// projections when their formatter is implemented.
        diagnostics: Vec<NativePolicyStartupFatal>,
    },
    /// The requested source does not match the active helper plan or its tier.
    SourceMismatch,
}

/// Result of source-aware helper settings composition.
#[derive(Debug, Clone, PartialEq)]
pub struct NativePolicyHelperApplyResult {
    /// Whether the helper settings were applied or why they were not.
    pub status: NativePolicyHelperApplyStatus,
    /// Source authorized by the active helper plan.
    pub source: NativePolicySource,
    /// Effective selection after application, or the unchanged input on
    /// rejection/unresolved input.
    pub selection: NativePolicySelection,
    /// Legacy bounded parser diagnostics; source-faithful rows are below.
    pub diagnostics: Vec<NativePolicyParseDiagnostic>,
    /// Exact Native `At` warning rows for this applied helper/fallback value.
    pub source_warnings: Vec<Utf16JsonProjection>,
    /// Warning branches whose Native row formatter is not represented.
    pub warning_unresolved_fields: Vec<String>,
}

/// Distinguishes Native's dynamic helper output from a named static fallback.
/// Static rows are looked up by exact field identity and applied only from the
/// Core-normalized candidate; a caller cannot nominate raw settings as static.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyHelperSettingsInput {
    /// Runtime helper envelope content, already admitted by `Hi()`.
    Dynamic {
        /// Missing `managedSettings` means a valid helper output with no policy
        /// subtree; its prompt fields may still be retained by the Host.
        settings: Option<Utf16JsonProjection>,
        /// Native helper-level composition mode.
        behavior: NativePolicyHelperOutputBehavior,
        /// Exact Bun 1.4.3 results for the source-derived operations reachable
        /// in this managedSettings projection.
        semantic_evidence: NativePolicySemanticEvidence,
    },
    /// A specific `policyHelpers.*.defaultSettings` source selected by Native.
    StaticFallback { source_field: String },
}

/// One static fallback in Native `er()` source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyStaticFallbackCandidate {
    /// Native source location, such as `policyHelpers.macos.defaultSettings`.
    pub source_field: String,
    /// Exact settings payload projection, unchanged from its source.
    pub settings: Utf16JsonProjection,
    /// The `At`/`Due`/`O_` result. Only this value may be applied.
    pub normalized_settings: Option<Utf16JsonProjection>,
    /// Exact `xi()` startup-fatal message when source-formattable.
    pub diagnostic: Option<Utf16JsonProjection>,
    /// Exact warning rows returned by Native `tr()`, including
    /// `statusOnly:true`.
    pub warnings: Vec<Utf16JsonProjection>,
    /// Warning branches with no source-equivalent row formatter yet.
    pub warning_unresolved_fields: Vec<String>,
    /// Exact JS RegExp patterns still requiring the Runtime Bun validator.
    pub regex_patterns: Vec<Vec<u16>>,
    /// Exact semantic worker inputs required to validate this raw candidate.
    pub semantic_inputs: NativePolicySemanticInputs,
    /// Whether this payload is on the current platform chain or map default.
    pub on_chain: bool,
    /// Whether Native applies this payload as a merge over its source.
    pub merges_output: bool,
    /// Admission state. A host must not apply an unresolved or rejected raw
    /// payload as though it were validated Native settings.
    pub validation: NativePolicyStaticFallbackValidation,
}

/// Re-run every retained static candidate after the host has obtained the
/// exact typed results for the source strings in those candidates.
///
/// The selector itself is synchronous and does not start JavaScript. Before
/// this call, candidates requiring RegExp, URL, or display observations remain
/// unresolved. This function replaces those provisional results atomically.
pub fn revalidate_native_policy_static_fallbacks(
    selection: &mut NativePolicySelection,
    evidence: &NativePolicySemanticEvidence,
) {
    let Some(plan) = selection.helper_plan.as_mut() else {
        return;
    };
    let source = plan.source;
    let source_paths = plan.source_paths.clone();
    for candidate in &mut plan.static_fallback_candidates {
        *candidate = validate_static_fallback_candidate(
            candidate.source_field.clone(),
            candidate.settings.clone(),
            candidate.on_chain,
            candidate.merges_output,
            evidence,
        );
    }
    let fatal_candidate = plan
        .static_fallback_candidates
        .iter()
        .find_map(|candidate| {
            (candidate.validation == NativePolicyStaticFallbackValidation::Rejected)
                .then(|| {
                    candidate
                        .diagnostic
                        .clone()
                        .map(|message| (candidate.source_field.clone(), message))
                })
                .flatten()
        });
    selection.startup_fatal_helper_paths = collect_startup_fatal_helper_paths(&selection.documents);
    if selection.startup_fatal_helper_paths.is_empty() {
        if let Some((path, diagnostic)) = fatal_candidate {
            selection
                .startup_fatal_helper_paths
                .push(NativePolicyStartupFatal {
                    source,
                    source_paths,
                    path,
                    diagnostic: Some(diagnostic),
                });
        }
    }
}

/// Source-bound Native helper selection metadata for the selected admin tier.
/// It does not mean a helper has executed or that any fallback has been
/// applied; the host executor owns that second stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyHelperPlan {
    /// Winning managed source whose helper configuration is eligible.
    pub source: NativePolicySource,
    /// Physical source paths represented by the winning source tier.
    pub source_paths: Vec<PathBuf>,
    /// MDM plist writability fact, if the winning source is a plist.
    pub source_user_writable: bool,
    /// Host platform used to compute the Native OS key chain.
    pub platform: NativePolicyPlatform,
    /// Ordered platform keys checked by Native (`wsl`, then `linux` for WSL).
    pub platform_chain: Vec<String>,
    /// Normalized singular helper config, if present on this source.
    pub policy_helper: Option<Utf16JsonProjection>,
    /// Normalized per-OS config map, if present on this source.
    pub policy_helpers: Option<Utf16JsonProjection>,
    /// First per-OS helper entry selected by Native's platform chain.
    pub selected_entry: Option<NativePolicyHelperEntry>,
    /// Singular helper considered only after per-OS helpers and static
    /// fallback payloads.
    pub singular_entry: Option<NativePolicyHelperEntry>,
    /// Native's selected source branch; this is metadata, not execution.
    pub selected_kind: NativePolicyHelperSelectedKind,
    /// Source field of the first on-chain static fallback, if one exists.
    pub selected_static_fallback_field: Option<String>,
    /// Whether this helper plan is safe to use under the implemented parser.
    pub validation: NativePolicyHelperPlanValidation,
    /// Source diagnostics marked `startupFatal` by the Native policy parser.
    /// A host must refuse policy-helper startup while this list is nonempty.
    pub startup_fatal_paths: Vec<String>,
    /// Static fallback candidates in Native `er()` order.
    pub static_fallback_candidates: Vec<NativePolicyStaticFallbackCandidate>,
}

/// One source-bound Native helper-policy `startupFatal` diagnostic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyStartupFatal {
    /// Policy source that produced this diagnostic.
    pub source: NativePolicySource,
    /// Physical source path(s) for the document.
    pub source_paths: Vec<PathBuf>,
    /// Native field path, such as `policyHelpers.linux`.
    pub path: String,
    /// Exact Native load-error message projection, when source-represented.
    /// `None` is retained for Zod issue branches whose formatter is not
    /// ported; callers must not synthesize a Native-like replacement.
    pub diagnostic: Option<Utf16JsonProjection>,
}

/// Provenance associated with one plist candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyPlistFacts {
    /// Exact plist path selected by the Native reader.
    pub path: PathBuf,
    /// Whether this plist is writable by the current user account.
    pub user_writable: bool,
    /// Reader state for this candidate.
    pub load_state: NativePolicyLoadState,
    /// Parser-provided Native `documentHasPolicyContent` fact, when available.
    pub document_has_policy_content: Option<bool>,
}

/// Provenance associated with one managed-settings file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyManagedFileFacts {
    /// Path of this file in the managed-settings source order.
    pub path: PathBuf,
    /// Reader state for this file.
    pub load_state: NativePolicyLoadState,
    /// Parser-provided Native `documentHasPolicyContent` fact, when available.
    pub document_has_policy_content: Option<bool>,
}

/// Provenance associated with a parent-managed settings document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyParentFacts {
    /// Stable identity of the host slot that supplied the parent document.
    pub provider: String,
}

/// Provenance associated with a local host fallback document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyHostFacts {
    /// Which host fallback contract produced the document.
    pub kind: NativePolicyHostKind,
    /// Stable local source identity, such as the registry key or host slot.
    pub identity: String,
    /// Reader state for the source.
    pub load_state: NativePolicyLoadState,
}

/// Native control-field values found in the exact document projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativePolicyControls {
    /// `managedSourcesBehavior`, defaulting to Native's `first-wins`.
    pub managed_sources_behavior: NativeManagedSourcesBehavior,
    /// `parentSettingsBehavior`, defaulting to Native's ordinary
    /// `first-wins` behavior when no remote override applies.
    pub parent_settings_behavior: NativeParentSettingsBehavior,
}

/// `managedSourcesBehavior` from the highest active admin source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeManagedSourcesBehavior {
    /// Only the winning admin source contributes ordinary values.
    FirstWins,
    /// Active admin sources are composed in Native source order.
    Merge,
}

/// `parentSettingsBehavior` from the selected admin source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeParentSettingsBehavior {
    /// Parent settings do not enter the result unless Native's
    /// `parentNeverShutOut` gate admits the restrictive slice.
    FirstWins,
    /// The filtered restrictive parent slice is merged below admin settings.
    Merge,
}

/// Admission status for a retained source document.
#[derive(Debug, Clone, PartialEq)]
pub enum NativePolicyAdmission {
    /// Native's structural JSON/root check and policy controls admitted this
    /// document. The current typed Core view may be absent without changing
    /// source selection.
    NativePolicyAccepted {
        /// Optional typed view for current Core consumers.
        settings: Option<SettingsJson>,
        controls: NativePolicyControls,
    },
    /// The source failed Native structural admission or a policy-control
    /// validator. Its original projection remains available on the document.
    NativePolicyRejected { reason: String },
    /// The source did not produce a JSON projection.
    Unavailable {
        /// Reader state supplied by the source adapter.
        load_state: NativePolicyLoadState,
    },
}

/// One source document, preserving its raw input and admission provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct NativePolicyDocument {
    source: NativePolicySource,
    origin: NativePolicyOrigin,
    raw_projection: Option<Utf16JsonProjection>,
    candidate_projection: Option<Utf16JsonProjection>,
    normalized_projection: Option<Utf16JsonProjection>,
    document_has_policy_content: Option<bool>,
    only_substitutes: bool,
    unresolved_fields: Vec<String>,
    startup_fatal_paths: Vec<String>,
    startup_fatal_diagnostics: Vec<NativePolicyStartupFatalDiagnostic>,
    helper_diagnostics: Vec<NativePolicyHelperDiagnostic>,
    parse_diagnostics: Vec<NativePolicyParseDiagnostic>,
    admission: NativePolicyAdmission,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum NativePolicyOrigin {
    Plist(NativePolicyPlistFacts),
    ManagedFile(NativePolicyManagedFileFacts),
    Parent(NativePolicyParentFacts),
    Host(NativePolicyHostFacts),
}

impl NativePolicyDocument {
    /// Parse and admit one plist reader result while retaining the original
    /// source projection. `None` represents the reader's absent/unreadable
    /// result and is kept as provenance rather than converted into `{}`.
    #[must_use]
    pub fn parse_plist(
        raw_projection: Option<Utf16JsonProjection>,
        facts: NativePolicyPlistFacts,
    ) -> Self {
        Self::parse(
            NativePolicySource::Plist,
            NativePolicyOrigin::Plist(facts),
            raw_projection,
            false,
        )
    }

    /// Parse one managed-settings file. Callers pass documents in Native's
    /// file order: `managed-settings.json`, followed by sorted drop-in files.
    #[must_use]
    pub fn parse_managed_file(
        raw_projection: Option<Utf16JsonProjection>,
        facts: NativePolicyManagedFileFacts,
    ) -> Self {
        Self::parse(
            NativePolicySource::ManagedFile,
            NativePolicyOrigin::ManagedFile(facts),
            raw_projection,
            false,
        )
    }

    /// Parse a parent-managed settings document. Parent values are later
    /// reduced to Native's restrictive allowlist before composition.
    #[must_use]
    pub fn parse_parent(
        raw_projection: Option<Utf16JsonProjection>,
        facts: NativePolicyParentFacts,
    ) -> Self {
        Self::parse(
            NativePolicySource::Parent,
            NativePolicyOrigin::Parent(facts),
            raw_projection,
            true,
        )
    }

    /// Parse a local host fallback. Native consults this only when no admin
    /// policy and no parent slice was selected.
    #[must_use]
    pub fn parse_host(
        raw_projection: Option<Utf16JsonProjection>,
        facts: NativePolicyHostFacts,
    ) -> Self {
        Self::parse(
            NativePolicySource::Host,
            NativePolicyOrigin::Host(facts),
            raw_projection,
            false,
        )
    }

    fn parse(
        source: NativePolicySource,
        origin: NativePolicyOrigin,
        raw_projection: Option<Utf16JsonProjection>,
        strip_parent_controls: bool,
    ) -> Self {
        let Some(raw) = raw_projection.as_ref() else {
            let load_state = origin.load_state();
            let document_has_policy_content = origin.document_has_policy_content();
            return Self {
                source,
                origin,
                raw_projection,
                candidate_projection: None,
                normalized_projection: None,
                document_has_policy_content,
                only_substitutes: false,
                unresolved_fields: Vec::new(),
                startup_fatal_paths: Vec::new(),
                startup_fatal_diagnostics: Vec::new(),
                helper_diagnostics: Vec::new(),
                parse_diagnostics: Vec::new(),
                admission: NativePolicyAdmission::Unavailable { load_state },
            };
        };

        let raw_document = raw.to_json_string().unwrap_or_default();
        let parser_result = parse_policy_projection(raw_document, raw.clone());
        let mut candidate_projection = parser_result.candidate_projection;
        let mut normalized_projection = parser_result.normalized_projection;
        if strip_parent_controls {
            strip_parent_policy_controls(&mut candidate_projection);
            strip_parent_policy_controls(&mut normalized_projection);
        }
        let document_has_policy_content = origin
            .document_has_policy_content()
            .or(parser_result.document_has_policy_content);
        let only_substitutes = parser_result.only_substitutes;
        let unresolved_fields = parser_result.unresolved_fields;
        let startup_fatal_paths = parser_result.startup_fatal_paths;
        let startup_fatal_diagnostics = parser_result.startup_fatal_diagnostics;
        let helper_diagnostics = parser_result.helper_diagnostics;
        let parse_diagnostics = parser_result.diagnostics;
        let admission = if !parser_result.document_admitted {
            NativePolicyAdmission::NativePolicyRejected {
                reason: match parser_result.status {
                    #[cfg(test)]
                    NativePolicyParseStatus::RejectedInvalidJson(reason) => reason,
                    NativePolicyParseStatus::RejectedNonObjectRoot => {
                        "policy document root must be an object".into()
                    }
                    NativePolicyParseStatus::ParsedObject => {
                        "policy parser did not admit the document".into()
                    }
                },
            }
        } else if let Some(projection) = normalized_projection.as_ref() {
            match parse_controls(projection) {
                Ok(controls) => NativePolicyAdmission::NativePolicyAccepted {
                    settings: parse_typed_settings(projection),
                    controls,
                },
                Err(reason) => NativePolicyAdmission::NativePolicyRejected { reason },
            }
        } else {
            NativePolicyAdmission::NativePolicyRejected {
                reason: "policy parser admitted an object without a normalized projection".into(),
            }
        };
        Self {
            source,
            origin,
            raw_projection,
            candidate_projection,
            normalized_projection,
            document_has_policy_content,
            only_substitutes,
            unresolved_fields,
            startup_fatal_paths,
            startup_fatal_diagnostics,
            helper_diagnostics,
            parse_diagnostics,
            admission,
        }
    }

    /// Exact source category.
    #[must_use]
    pub const fn source(&self) -> NativePolicySource {
        self.source
    }

    /// The unchanged source JSON, if the reader produced one.
    #[must_use]
    pub fn raw_projection(&self) -> Option<&Utf16JsonProjection> {
        self.raw_projection.as_ref()
    }

    /// The source-transformed candidate JSON, if present. For user-writable
    /// plists, this reflects Native's source-level filtering while
    /// `raw_projection` remains exact.
    #[must_use]
    pub fn candidate_projection(&self) -> Option<&Utf16JsonProjection> {
        self.candidate_projection.as_ref()
    }

    /// Native-normalized policy projection, if structural and field parsing
    /// produced one.
    #[must_use]
    pub fn normalized_projection(&self) -> Option<&Utf16JsonProjection> {
        self.normalized_projection.as_ref()
    }

    /// Whether the supported Native parser classified this document as
    /// containing only substituted/synthesized policy fields.
    #[must_use]
    pub const fn only_substitutes(&self) -> bool {
        self.only_substitutes
    }

    /// Native-special policy fields whose exact parser behavior remains
    /// unresolved for this document.
    #[must_use]
    pub fn unresolved_fields(&self) -> &[String] {
        &self.unresolved_fields
    }

    /// Native policy paths whose diagnostics carry `startupFatal`.
    #[must_use]
    pub fn startup_fatal_paths(&self) -> &[String] {
        &self.startup_fatal_paths
    }

    /// Exact source messages for the startup-fatal paths present on this
    /// document, when the Native issue formatter is source-represented.
    #[must_use]
    pub fn startup_fatal_diagnostics(&self) -> &[NativePolicyStartupFatalDiagnostic] {
        &self.startup_fatal_diagnostics
    }

    /// Exact helper status and validation diagnostics in Native traversal
    /// order. These are distinct from generic parser outcomes because the
    /// helper path retains Native's source-specific wording.
    #[must_use]
    pub fn helper_diagnostics(&self) -> &[NativePolicyHelperDiagnostic] {
        &self.helper_diagnostics
    }

    /// Parser source events retained for the source-aware host error projection.
    #[must_use]
    pub fn parse_diagnostics(&self) -> &[NativePolicyParseDiagnostic] {
        &self.parse_diagnostics
    }

    /// Admission result for this exact candidate.
    #[must_use]
    pub const fn admission(&self) -> &NativePolicyAdmission {
        &self.admission
    }

    /// Source metadata including path, user-writable status, and load state.
    #[must_use]
    pub const fn source_metadata(&self) -> NativePolicySourceMetadata<'_> {
        match &self.origin {
            NativePolicyOrigin::Plist(facts) => NativePolicySourceMetadata::Plist(facts),
            NativePolicyOrigin::ManagedFile(facts) => {
                NativePolicySourceMetadata::ManagedFile(facts)
            }
            NativePolicyOrigin::Parent(facts) => NativePolicySourceMetadata::Parent(facts),
            NativePolicyOrigin::Host(facts) => NativePolicySourceMetadata::Host(facts),
        }
    }

    fn controls(&self) -> Option<NativePolicyControls> {
        match &self.admission {
            NativePolicyAdmission::NativePolicyAccepted { controls, .. } => Some(*controls),
            NativePolicyAdmission::NativePolicyRejected { .. }
            | NativePolicyAdmission::Unavailable { .. } => None,
        }
    }

    fn user_writable(&self) -> bool {
        match &self.origin {
            NativePolicyOrigin::Plist(facts) => facts.user_writable,
            NativePolicyOrigin::ManagedFile(_)
            | NativePolicyOrigin::Parent(_)
            | NativePolicyOrigin::Host(_) => false,
        }
    }

    fn can_supply_admin_policy_helpers(&self) -> bool {
        match &self.origin {
            NativePolicyOrigin::Plist(facts) => !facts.user_writable,
            NativePolicyOrigin::ManagedFile(_) => true,
            NativePolicyOrigin::Parent(_) | NativePolicyOrigin::Host(_) => false,
        }
    }

    fn load_state(&self) -> NativePolicyLoadState {
        self.origin.load_state()
    }

    fn has_possible_policy_content(&self) -> bool {
        match self.document_has_policy_content {
            Some(value) => value,
            None => self
                .normalized_projection
                .as_ref()
                .is_some_and(native_has_policy_content),
        }
    }

    fn only_substitutes_is_exact(&self) -> bool {
        self.only_substitutes && self.unresolved_fields.is_empty()
    }

    fn source_paths(&self) -> Vec<PathBuf> {
        match &self.origin {
            NativePolicyOrigin::Plist(facts) => vec![facts.path.clone()],
            NativePolicyOrigin::ManagedFile(facts) => vec![facts.path.clone()],
            NativePolicyOrigin::Parent(_) | NativePolicyOrigin::Host(_) => Vec::new(),
        }
    }
}

/// Borrowed source metadata view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicySourceMetadata<'a> {
    /// Plist candidate metadata.
    Plist(&'a NativePolicyPlistFacts),
    /// Managed-file metadata.
    ManagedFile(&'a NativePolicyManagedFileFacts),
    /// Parent provider metadata.
    Parent(&'a NativePolicyParentFacts),
    /// Host fallback metadata.
    Host(&'a NativePolicyHostFacts),
}

impl NativePolicyOrigin {
    fn load_state(&self) -> NativePolicyLoadState {
        match self {
            Self::Plist(facts) => facts.load_state,
            Self::ManagedFile(facts) => facts.load_state,
            Self::Parent(_) => NativePolicyLoadState::Loaded,
            Self::Host(facts) => facts.load_state,
        }
    }

    fn document_has_policy_content(&self) -> Option<bool> {
        match self {
            Self::Plist(facts) => facts.document_has_policy_content,
            Self::ManagedFile(facts) => facts.document_has_policy_content,
            Self::Parent(_) | Self::Host(_) => None,
        }
    }
}

/// Source documents supplied to the Native managed-policy selector.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NativePolicyInputs {
    /// Actual host platform used to select the Native per-OS helper chain.
    pub platform: NativePolicyPlatform,
    /// Plist candidates in Native reader order. On macOS this is the per-user
    /// candidate followed by the device candidate, already truncated by the
    /// reader at its first non-null `plutil` stdout.
    pub plist_candidates: Vec<NativePolicyDocument>,
    /// Local managed-settings files in source order.
    pub managed_file_documents: Vec<NativePolicyDocument>,
    /// Parent-managed settings, if the embedding host actually supplies them.
    pub parent: Option<NativePolicyDocument>,
    /// Local host/HKCU fallback. Darwin callers leave this absent.
    pub host: Option<NativePolicyDocument>,
}

/// Selected effective managed policy plus all per-document provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct NativePolicySelection {
    /// Effective policy projection. `None` means the native composition path
    /// produced no policy document.
    pub effective: Option<NativePolicyEffective>,
    /// Winning admin source, if any.
    pub selected_admin: Option<NativePolicySource>,
    /// Native helper metadata extracted only from the winning admin source.
    /// `None` means that source supplied no helper fields. This is not proof
    /// that a helper executed or a static fallback was applied.
    pub helper_plan: Option<NativePolicyHelperPlan>,
    /// Startup-fatal helper diagnostics from every local admin candidate,
    /// including non-winning sources. Native checks these load errors before
    /// it runs any helper.
    pub startup_fatal_helper_paths: Vec<NativePolicyStartupFatal>,
    /// Exact warning rows associated with the currently applied helper output.
    pub helper_warnings: Vec<Utf16JsonProjection>,
    /// Warning branches whose source rows are not yet represented.
    pub helper_warning_unresolved_fields: Vec<String>,
    /// Source with a parsed policy-content marker but no admitted policy keys.
    /// Native holds this empty admin result to prevent an unrelated host
    /// fallback from becoming effective.
    pub held_empty_source: Option<NativePolicySource>,
    /// Admin sources that contributed ordinary values, high priority first.
    pub admin_sources: Vec<NativePolicySource>,
    /// Whether Native's restrictive parent slice entered the composition.
    pub parent_included: bool,
    /// Whether the Host/HKCU fallback supplied the effective result.
    pub host_fallback_used: bool,
    /// False when a document was rejected/unavailable or a present
    /// Native-special field still has unresolved parser behavior. True is
    /// scoped to the structural and field-specific parsers represented here;
    /// it is not a claim that every Native settings validator is implemented.
    pub complete: bool,
    /// Original documents, including rejected and unavailable source evidence.
    pub documents: Vec<NativePolicyDocument>,
    /// Structural source errors and Native-special fields that remain
    /// unresolved for the current parser.
    pub unresolved: Vec<NativePolicyUnresolved>,
    /// Immutable base inputs needed to recompose helper output without
    /// applying a refresh repeatedly over the previous helper result.
    composition: NativePolicySelectionComposition,
}

/// Effective projection and current Core typed view.
#[derive(Debug, Clone, PartialEq)]
pub struct NativePolicyEffective {
    /// Source-preserving merged projection.
    pub projection: Utf16JsonProjection,
    /// Current Core typed parse of `projection`.
    pub settings: Option<SettingsJson>,
}

/// Admission or reader gap associated with a source document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyUnresolved {
    /// Source category for the unresolved document.
    pub source: NativePolicySource,
    /// Human-readable bounded reason.
    pub reason: String,
}

/// Select admin tiers, apply Native restrictive settings, reduce parent
/// settings to their restrictive subset, then consult the host fallback.
#[must_use]
pub fn resolve_native_policy(inputs: NativePolicyInputs) -> NativePolicySelection {
    let mut documents = inputs.plist_candidates.clone();
    documents.extend(inputs.managed_file_documents.iter().cloned());
    if let Some(parent) = &inputs.parent {
        documents.push(parent.clone());
    }
    if let Some(host) = &inputs.host {
        documents.push(host.clone());
    }

    let mut unresolved = documents
        .iter()
        .flat_map(unresolved_document)
        .collect::<Vec<_>>();

    let plist = select_plist_candidate(&inputs.plist_candidates, &mut unresolved);
    let file = compose_managed_files(&inputs.managed_file_documents);
    let file_only_substitutes = managed_files_only_substitutes(&inputs.managed_file_documents);

    let mut admin_candidates = Vec::new();
    if let Some(plist) = plist.as_ref() {
        if plist
            .normalized_projection
            .as_ref()
            .is_some_and(native_has_policy_content)
        {
            admin_candidates.push(AdminCandidate {
                source: NativePolicySource::Plist,
                source_paths: plist.source_paths(),
                source_user_writable: plist.user_writable(),
                projection: plist
                    .normalized_projection
                    .as_ref()
                    .expect("admitted plist has projection")
                    .clone(),
                unresolved_fields: plist.unresolved_fields().to_vec(),
                startup_fatal_paths: plist.startup_fatal_paths().to_vec(),
                rank: candidate_rank(plist),
                controls: plist.controls().expect("admitted plist has controls"),
            });
        }
    }
    if let Some(file_projection) = file.as_ref() {
        if native_has_policy_content(file_projection) {
            match parse_controls(file_projection) {
                Ok(controls) => admin_candidates.push(AdminCandidate {
                    source: NativePolicySource::ManagedFile,
                    source_paths: managed_file_source_paths(&inputs.managed_file_documents),
                    source_user_writable: false,
                    projection: file_projection.clone(),
                    unresolved_fields: managed_file_unresolved_fields(
                        &inputs.managed_file_documents,
                    ),
                    startup_fatal_paths: managed_file_startup_fatal_paths(
                        &inputs.managed_file_documents,
                    ),
                    rank: candidate_rank_for_projection(file_projection, file_only_substitutes),
                    controls,
                }),
                Err(reason) => {
                    unresolved.push(NativePolicyUnresolved {
                        source: NativePolicySource::ManagedFile,
                        reason,
                    });
                }
            }
        }
    }

    // Native's `xR` compares source ranks and uses `>=`, so remote beats a tie,
    // then plist beats a tie with the local managed file. Remote production is
    // intentionally not supplied by this current local-input API.
    admin_candidates.sort_by_key(|candidate| match candidate.source {
        NativePolicySource::Plist => 0,
        NativePolicySource::ManagedFile => 1,
        NativePolicySource::Remote => -1,
        NativePolicySource::Parent | NativePolicySource::Host => 2,
    });
    let selected_index = admin_candidates
        .iter()
        .enumerate()
        .max_by_key(|(_, candidate)| (candidate.rank, -source_tie_rank(candidate.source)))
        .map(|(index, _)| index);
    let selected_admin = selected_index.map(|index| admin_candidates[index].source);
    let helper_plan = selected_index.and_then(|index| {
        let candidate = &admin_candidates[index];
        native_policy_helper_plan(candidate, inputs.platform)
    });
    let selected_controls = selected_index.map(|index| admin_candidates[index].controls);
    let behavior = admin_candidates
        .iter()
        .find(|candidate| {
            candidate
                .projection
                .get_property("managedSourcesBehavior")
                .is_some()
                || candidate.rank == 2
        })
        .map(|candidate| candidate.controls.managed_sources_behavior)
        .unwrap_or(NativeManagedSourcesBehavior::FirstWins);

    let contributing = match (selected_index, behavior) {
        (Some(_), NativeManagedSourcesBehavior::Merge) => admin_candidates.clone(),
        (Some(selected), NativeManagedSourcesBehavior::FirstWins) => {
            vec![admin_candidates[selected].clone()]
        }
        (None, _) => Vec::new(),
    };
    let mut admin_projection = if contributing.is_empty() {
        None
    } else {
        Some(compose_admin_tiers(
            &contributing,
            selected_index.map(|index| &admin_candidates[index].projection),
        ))
    };

    let parent_never_shut_out = !admin_candidates.iter().any(|candidate| {
        candidate.rank == 2
            && matches!(
                candidate.source,
                NativePolicySource::Plist | NativePolicySource::ManagedFile
            )
    });
    let parent_settings = inputs.parent.as_ref().and_then(|document| {
        document
            .controls()
            .and(document.normalized_projection.as_ref())
    });
    let held_empty_source = if selected_admin.is_none() {
        inputs
            .plist_candidates
            .iter()
            .find(|document| document.has_possible_policy_content())
            .map(|_| NativePolicySource::Plist)
            .or_else(|| {
                inputs
                    .managed_file_documents
                    .iter()
                    .find(|document| document.has_possible_policy_content())
                    .map(|_| NativePolicySource::ManagedFile)
            })
    } else {
        None
    };
    let parent_included = parent_settings.is_some()
        && (selected_admin.is_none()
            || selected_controls.is_some_and(|controls| {
                controls.parent_settings_behavior == NativeParentSettingsBehavior::Merge
            })
            || parent_never_shut_out);

    let parent_slice = if parent_included {
        let parent = parent_settings.expect("included parent has projection");
        let admin_context = admin_projection.as_ref();
        Some(parent_restrictive_slice(parent, admin_context))
    } else {
        None
    };

    let host_fallback_projection = inputs.host.as_ref().and_then(|document| {
        document
            .controls()
            .and(document.normalized_projection.as_ref())
            .filter(|projection| !is_empty_object(projection))
            .cloned()
    });

    let (effective_projection, host_fallback_used) = match (admin_projection.take(), parent_slice) {
        (Some(admin), Some(parent)) => (
            Some(native_merge(
                &parent,
                &strip_managed_sources_behavior(&admin),
            )),
            false,
        ),
        (Some(admin), None) => (Some(strip_managed_sources_behavior(&admin)), false),
        (None, Some(parent)) if !is_empty_object(&parent) => (Some(parent), false),
        (None, _) if held_empty_source.is_some() => (Some(empty_object()), false),
        (None, _) => (
            host_fallback_projection.clone(),
            host_fallback_projection.is_some(),
        ),
    };

    let effective = effective_projection.map(|projection| {
        let settings = parse_typed_settings(&projection);
        NativePolicyEffective {
            projection,
            settings,
        }
    });

    let admin_sources = contributing
        .iter()
        .map(|candidate| candidate.source)
        .collect();
    let mut startup_fatal_helper_paths = collect_startup_fatal_helper_paths(&documents);
    if startup_fatal_helper_paths.is_empty() {
        if let Some(plan) = helper_plan.as_ref() {
            if let Some(candidate) = plan.static_fallback_candidates.iter().find(|candidate| {
                candidate.validation != NativePolicyStaticFallbackValidation::Validated
            }) {
                if let NativePolicyStaticFallbackValidation::Rejected = candidate.validation {
                    if let Some(diagnostic) = candidate.diagnostic.clone() {
                        startup_fatal_helper_paths.push(NativePolicyStartupFatal {
                            source: plan.source,
                            source_paths: plan.source_paths.clone(),
                            path: candidate.source_field.clone(),
                            diagnostic: Some(diagnostic),
                        });
                    }
                }
            }
        }
    }
    let composition = NativePolicySelectionComposition {
        admin_tiers: contributing.clone(),
        helper_source_projection: selected_index
            .filter(|index| !admin_candidates[*index].source_user_writable)
            .map(|index| admin_candidates[index].projection.clone()),
        managed_sources_behavior: behavior,
        parent_projection: parent_settings.cloned(),
        parent_included,
        parent_never_shut_out,
        host_fallback_projection,
        host_fallback_used,
    };
    NativePolicySelection {
        effective,
        selected_admin,
        helper_plan,
        startup_fatal_helper_paths,
        helper_warnings: Vec::new(),
        helper_warning_unresolved_fields: Vec::new(),
        held_empty_source,
        admin_sources,
        parent_included,
        host_fallback_used,
        complete: unresolved.is_empty(),
        documents,
        unresolved,
        composition,
    }
}

/// Parse the source-backed `Hi()` helper stdout envelope without converting
/// its string fields through `serde_json::Value`.
pub fn parse_native_policy_helper_output(
    raw: &Utf16JsonProjection,
) -> Result<NativePolicyHelperOutput, NativePolicyHelperOutputError> {
    let Some(root) = raw.at_pointer("") else {
        return Err(helper_non_object_root_error());
    };
    if root.object_entries().is_none() {
        if root.array_len().is_some() {
            return Err(NativePolicyHelperOutputError::InvalidEnvelope {
                diagnostic: helper_envelope_type_error("object", "array", &[]),
            });
        }
        return Err(helper_non_object_root_error());
    }

    let managed_settings = helper_envelope_field(raw, root, "managedSettings")?;
    let claude_md = helper_envelope_field(raw, root, "claudeMd")?;
    let append_system_prompt = helper_envelope_field(raw, root, "appendSystemPrompt")?;
    let mut invalid_fields = Vec::new();
    if let Some(value) = claude_md.as_ref() {
        if value
            .at_pointer("")
            .and_then(JsonValueView::code_units)
            .is_none()
        {
            invalid_fields.push(("claudeMd", helper_json_type(value)));
        }
    }
    if let Some(value) = append_system_prompt.as_ref() {
        if value
            .at_pointer("")
            .and_then(JsonValueView::code_units)
            .is_none()
        {
            invalid_fields.push(("appendSystemPrompt", helper_json_type(value)));
        }
    }
    if !invalid_fields.is_empty() {
        return Err(NativePolicyHelperOutputError::InvalidEnvelope {
            diagnostic: helper_envelope_fields_type_error(&invalid_fields),
        });
    }

    Ok(NativePolicyHelperOutput {
        raw: raw.clone(),
        managed_settings,
        claude_md,
        append_system_prompt,
    })
}

/// Apply Native's source-bound helper error text cleanup after VT escape
/// removal. Tab and line-feed are preserved; other Cc/Cf, U+2028, and U+2029
/// code points are removed. Rust strings are already well-formed UTF-8, so
/// this helper covers the Unicode-category portion of Native `zd` after its
/// preceding lone-surrogate cleanup.
pub fn sanitize_native_policy_helper_error_text(message: &str) -> String {
    use unicode_general_category::{get_general_category, GeneralCategory};

    super::native_policy_error_vt::strip_native_vt_control_characters(message)
        .chars()
        .filter(|character| {
            if matches!(*character, '\t' | '\n') {
                return true;
            }
            !matches!(
                get_general_category(*character),
                GeneralCategory::Control
                    | GeneralCategory::Format
                    | GeneralCategory::LineSeparator
                    | GeneralCategory::ParagraphSeparator
            )
        })
        .collect()
}

/// Apply one validated helper/static-fallback managed-settings subtree to the
/// winning admin tier and re-run the Native composition pipeline from its
/// immutable base layers.
///
/// The caller supplies the source identity from the validated helper plan and
/// the `outputBehavior` associated with the helper or static fallback. No
/// helper fields in `settings` are allowed to create another execution plan.
#[must_use]
pub fn apply_native_policy_helper_settings(
    selection: &NativePolicySelection,
    source: NativePolicySource,
    input: &NativePolicyHelperSettingsInput,
) -> NativePolicyHelperApplyResult {
    let unchanged = |status, diagnostics| NativePolicyHelperApplyResult {
        status,
        source,
        selection: selection.clone(),
        diagnostics,
        source_warnings: Vec::new(),
        warning_unresolved_fields: Vec::new(),
    };
    if !selection.startup_fatal_helper_paths.is_empty() {
        return unchanged(
            NativePolicyHelperApplyStatus::StartupFatal {
                diagnostics: selection.startup_fatal_helper_paths.clone(),
            },
            Vec::new(),
        );
    }
    let Some(plan) = selection.helper_plan.as_ref() else {
        return unchanged(NativePolicyHelperApplyStatus::SourceMismatch, Vec::new());
    };
    if plan.source != source || selection.selected_admin != Some(source) {
        return unchanged(NativePolicyHelperApplyStatus::SourceMismatch, Vec::new());
    }
    if let NativePolicyHelperPlanValidation::Unresolved { fields } = &plan.validation {
        return unchanged(
            NativePolicyHelperApplyStatus::Unresolved {
                fields: fields.clone(),
            },
            Vec::new(),
        );
    }
    if let Some(candidate) = plan
        .static_fallback_candidates
        .iter()
        .find(|candidate| candidate.validation != NativePolicyStaticFallbackValidation::Validated)
    {
        let fields = match &candidate.validation {
            NativePolicyStaticFallbackValidation::Unresolved { fields } => fields.clone(),
            NativePolicyStaticFallbackValidation::Rejected => vec![candidate.source_field.clone()],
            NativePolicyStaticFallbackValidation::Validated => unreachable!(),
        };
        return unchanged(
            NativePolicyHelperApplyStatus::Unresolved { fields },
            Vec::new(),
        );
    }

    let (normalized, behavior, source_warnings, warning_unresolved_fields) = match input {
        NativePolicyHelperSettingsInput::Dynamic { settings: None, .. } => {
            return unchanged(NativePolicyHelperApplyStatus::NoManagedSettings, Vec::new());
        }
        NativePolicyHelperSettingsInput::Dynamic {
            settings: Some(settings),
            behavior,
            semantic_evidence,
        } => {
            let inputs = collect_native_policy_semantic_inputs_with_js(
                settings,
                Some(&semantic_evidence.js),
            );
            if !semantic_evidence.covers(&inputs) {
                return unchanged(
                    NativePolicyHelperApplyStatus::Unresolved {
                        fields: semantic_inputs_uncovered(
                            &inputs,
                            semantic_evidence,
                            "policyHelper",
                        ),
                    },
                    Vec::new(),
                );
            }
            let parsed = validate_native_helper_managed_settings(
                settings,
                "policyHelper",
                semantic_evidence,
            );
            let source_warnings = parsed.at_warnings.clone();
            let warning_unresolved_fields = parsed.warning_unresolved_fields.clone();
            let Some(normalized) = parsed.normalized.clone() else {
                match parsed.status {
                    NativeStaticSettingsStatus::Invalid => {
                        let diagnostic = native_ftr_settings_error(&parsed, "policyHelper");
                        if let Some(diagnostic) = diagnostic {
                            return unchanged(
                                NativePolicyHelperApplyStatus::Rejected {
                                    reason: "Native managedSettings validation rejected the value"
                                        .into(),
                                    diagnostic: Some(diagnostic),
                                },
                                Vec::new(),
                            );
                        }
                    }
                    NativeStaticSettingsStatus::Unresolved | NativeStaticSettingsStatus::Valid => {}
                }
                let fields = if parsed.unresolved_fields.is_empty() {
                    vec!["policyHelper.managedSettings".to_owned()]
                } else {
                    parsed.unresolved_fields
                };
                return unchanged(
                    NativePolicyHelperApplyStatus::Unresolved { fields },
                    Vec::new(),
                );
            };
            (
                normalized,
                *behavior,
                source_warnings,
                warning_unresolved_fields,
            )
        }
        NativePolicyHelperSettingsInput::StaticFallback { source_field } => {
            if plan.selected_static_fallback_field.as_deref() != Some(source_field.as_str()) {
                return unchanged(NativePolicyHelperApplyStatus::SourceMismatch, Vec::new());
            }
            let Some(candidate) = plan
                .static_fallback_candidates
                .iter()
                .find(|candidate| candidate.source_field == *source_field)
            else {
                return unchanged(NativePolicyHelperApplyStatus::SourceMismatch, Vec::new());
            };
            let Some(normalized) = candidate.normalized_settings.clone() else {
                return unchanged(
                    NativePolicyHelperApplyStatus::Unresolved {
                        fields: vec![source_field.clone()],
                    },
                    Vec::new(),
                );
            };
            let behavior = if candidate.merges_output {
                NativePolicyHelperOutputBehavior::Merge
            } else {
                NativePolicyHelperOutputBehavior::Replace
            };
            (
                normalized,
                behavior,
                candidate.warnings.clone(),
                candidate.warning_unresolved_fields.clone(),
            )
        }
    };

    let normalized = strip_policy_helper_fields(&normalized);
    let mut admin_tiers = selection.composition.admin_tiers.clone();
    let Some(selected_tier) = admin_tiers.iter_mut().find(|tier| tier.source == source) else {
        return unchanged(NativePolicyHelperApplyStatus::SourceMismatch, Vec::new());
    };
    selected_tier.projection = match behavior {
        NativePolicyHelperOutputBehavior::Replace => normalized,
        NativePolicyHelperOutputBehavior::Merge => {
            native_merge(&selected_tier.projection, &normalized)
        }
    };

    let mut updated = selection.clone();
    updated.helper_warnings = source_warnings.clone();
    updated.helper_warning_unresolved_fields = warning_unresolved_fields.clone();
    updated.recompose_helper_settings(admin_tiers);
    NativePolicyHelperApplyResult {
        status: NativePolicyHelperApplyStatus::Applied,
        source,
        selection: updated,
        diagnostics: Vec::new(),
        source_warnings,
        warning_unresolved_fields,
    }
}

fn helper_envelope_field(
    raw: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    field: &'static str,
) -> Result<Option<Utf16JsonProjection>, NativePolicyHelperOutputError> {
    if root.get_property(field).is_none() {
        return Ok(None);
    }
    raw.subprojection(&format!("/{field}"))
        .map(Some)
        .map_err(|_| NativePolicyHelperOutputError::ProjectionUnavailable { field })
}

fn helper_json_type(value: &Utf16JsonProjection) -> &'static str {
    let Some(value) = value.at_pointer("") else {
        return "null";
    };
    if value.is_null() {
        "null"
    } else if value.array_len().is_some() {
        "array"
    } else if value.object_entries().is_some() {
        "object"
    } else if value.code_units().is_some() {
        "string"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.as_number().is_some() {
        "number"
    } else {
        "unknown"
    }
}

fn helper_non_object_root_error() -> NativePolicyHelperOutputError {
    let message = "stdout is not a JSON object";
    NativePolicyHelperOutputError::NonObjectRoot {
        diagnostic: Utf16JsonProjection::root_string(
            message.to_owned(),
            message.encode_utf16().collect(),
        )
        .expect("Native helper root-error message is valid UTF-16"),
    }
}

fn helper_envelope_type_error(
    expected: &str,
    received: &str,
    path: &[&str],
) -> Utf16JsonProjection {
    helper_zod_invalid_type_errors(&[(expected, received, path.to_vec())])
}

fn helper_envelope_fields_type_error(fields: &[(&str, &str)]) -> Utf16JsonProjection {
    let issues = fields
        .iter()
        .map(|(field, received)| ("string", *received, vec![*field]))
        .collect::<Vec<_>>();
    helper_zod_invalid_type_errors(&issues)
}

fn helper_zod_invalid_type_errors(issues: &[(&str, &str, Vec<&str>)]) -> Utf16JsonProjection {
    let mut message = String::from("[");
    for (index, (expected, received, path)) in issues.iter().enumerate() {
        if index > 0 {
            message.push(',');
        }
        message.push_str("\n  {\n    \"expected\": ");
        message.push_str(&serde_json::to_string(expected).expect("expected type is valid JSON"));
        message.push_str(",\n    \"code\": \"invalid_type\",\n    \"path\": ");
        if path.is_empty() {
            message.push_str("[]");
        } else {
            message.push_str("[\n");
            for (path_index, segment) in path.iter().enumerate() {
                if path_index > 0 {
                    message.push_str(",\n");
                }
                message.push_str("      ");
                message
                    .push_str(&serde_json::to_string(segment).expect("schema path is valid JSON"));
            }
            message.push_str("\n    ]");
        }
        message.push_str(",\n    \"message\": \"Invalid input: expected ");
        message.push_str(expected);
        message.push_str(", received ");
        message.push_str(received);
        message.push_str("\"\n  }");
    }
    message.push_str("\n]");
    let units = message.encode_utf16().collect::<Vec<_>>();
    Utf16JsonProjection::root_string(message, units)
        .expect("Native helper envelope diagnostic is a valid UTF-16 string")
}

impl NativePolicySelection {
    fn recompose_helper_settings(&mut self, admin_tiers: Vec<AdminCandidate>) {
        let admin_projection = if admin_tiers.is_empty() {
            None
        } else {
            Some(compose_admin_tiers(
                &admin_tiers,
                self.composition.helper_source_projection.as_ref(),
            ))
        };
        let parent_slice = if self.composition.parent_included {
            self.composition
                .parent_projection
                .as_ref()
                .map(|parent| parent_restrictive_slice(parent, admin_projection.as_ref()))
        } else {
            None
        };
        let (effective_projection, host_fallback_used) = match (admin_projection, parent_slice) {
            (Some(admin), Some(parent)) => (
                Some(native_merge(
                    &parent,
                    &strip_managed_sources_behavior(&admin),
                )),
                false,
            ),
            (Some(admin), None) => (Some(strip_managed_sources_behavior(&admin)), false),
            (None, Some(parent)) if !is_empty_object(&parent) => (Some(parent), false),
            (None, _) if self.held_empty_source.is_some() => (Some(empty_object()), false),
            (None, _) => (
                self.composition.host_fallback_projection.clone(),
                self.composition.host_fallback_projection.is_some(),
            ),
        };
        self.effective = effective_projection.map(|projection| NativePolicyEffective {
            settings: parse_typed_settings(&projection),
            projection,
        });
        self.host_fallback_used = host_fallback_used;
        self.admin_sources = admin_tiers.iter().map(|tier| tier.source).collect();
    }
}

#[derive(Debug, Clone, PartialEq)]
struct AdminCandidate {
    source: NativePolicySource,
    source_paths: Vec<PathBuf>,
    source_user_writable: bool,
    projection: Utf16JsonProjection,
    unresolved_fields: Vec<String>,
    startup_fatal_paths: Vec<String>,
    rank: i8,
    controls: NativePolicyControls,
}

#[derive(Debug, Clone, PartialEq)]
struct NativePolicySelectionComposition {
    /// Ordered admin source tiers before helper output is applied.
    admin_tiers: Vec<AdminCandidate>,
    /// Original helper configuration from the selected source. It remains
    /// separate from the effective settings projection and is never derived
    /// from helper output.
    helper_source_projection: Option<Utf16JsonProjection>,
    /// The selected admin source's first-wins/merge decision.
    managed_sources_behavior: NativeManagedSourcesBehavior,
    /// Original parent source projection, before restrictive reduction.
    parent_projection: Option<Utf16JsonProjection>,
    /// Whether Native included the parent restrictive slice.
    parent_included: bool,
    /// Whether no authored admin source can shut out restrictive parent data.
    parent_never_shut_out: bool,
    /// Host fallback source projection, when that branch supplied the result.
    host_fallback_projection: Option<Utf16JsonProjection>,
    /// Whether the Host/HKCU fallback supplied the original result.
    host_fallback_used: bool,
}

fn source_tie_rank(source: NativePolicySource) -> i8 {
    match source {
        NativePolicySource::Remote => 0,
        NativePolicySource::Plist => 1,
        NativePolicySource::ManagedFile => 2,
        NativePolicySource::Parent | NativePolicySource::Host => 3,
    }
}

fn candidate_rank(document: &NativePolicyDocument) -> i8 {
    document
        .normalized_projection
        .as_ref()
        .map_or(-1, |projection| {
            candidate_rank_for_projection(projection, document.only_substitutes_is_exact())
        })
}

fn candidate_rank_for_projection(projection: &Utf16JsonProjection, only_substitutes: bool) -> i8 {
    if !native_has_policy_content(projection) {
        return 0;
    }
    if !native_has_nonempty_policy_content(projection) {
        return 0;
    }
    // Native's B5e excludes an exact onlySubstitutes document from rank 2,
    // while bs still gives its nonempty settings rank 1. Unresolved special
    // fields are conservatively treated as authored until their parser lands.
    if only_substitutes {
        1
    } else {
        2
    }
}

fn select_plist_candidate<'a>(
    candidates: &'a [NativePolicyDocument],
    unresolved: &mut Vec<NativePolicyUnresolved>,
) -> Option<&'a NativePolicyDocument> {
    let mut administrator_source_seen = false;
    for document in candidates {
        if document.source != NativePolicySource::Plist {
            unresolved.push(NativePolicyUnresolved {
                source: document.source,
                reason: "plist candidate list contains a different source identity".into(),
            });
            continue;
        }
        if document.user_writable() && administrator_source_seen {
            continue;
        }
        match document.controls() {
            Some(_) if document.has_possible_policy_content() => return Some(document),
            Some(_) => {
                if !document.user_writable()
                    && (document.has_possible_policy_content()
                        || document.load_state() == NativePolicyLoadState::DidNotLoad)
                {
                    administrator_source_seen = true;
                }
            }
            None => {
                if !document.user_writable()
                    && (document.has_possible_policy_content()
                        || document.load_state() == NativePolicyLoadState::DidNotLoad)
                {
                    administrator_source_seen = true;
                }
            }
        }
    }
    None
}

fn compose_managed_files(documents: &[NativePolicyDocument]) -> Option<Utf16JsonProjection> {
    let mut merged: Option<Utf16JsonProjection> = None;
    for document in documents {
        if document.source != NativePolicySource::ManagedFile {
            continue;
        }
        if document.controls().is_none() {
            continue;
        }
        let Some(projection) = document.normalized_projection.as_ref() else {
            continue;
        };
        merged = Some(match merged {
            Some(previous) => native_merge(&previous, projection),
            None => projection.clone(),
        });
    }
    merged
}

fn managed_files_only_substitutes(documents: &[NativePolicyDocument]) -> bool {
    let mut has_policy = false;
    for document in documents {
        let Some(projection) = document.normalized_projection.as_ref() else {
            continue;
        };
        if document.controls().is_none() || !native_has_policy_content(projection) {
            continue;
        }
        if native_has_nonempty_policy_content(projection) {
            has_policy = true;
            if !document.only_substitutes_is_exact() {
                return false;
            }
        }
    }
    has_policy
}

fn compose_admin_tiers(
    tiers: &[AdminCandidate],
    helper_source: Option<&Utf16JsonProjection>,
) -> Utf16JsonProjection {
    let mut selected = empty_object();
    for tier in tiers.iter().rev() {
        selected = native_merge(&selected, &strip_policy_helper_fields(&tier.projection));
    }
    apply_admin_restrictions(&mut selected, tiers);
    let mut result = strip_managed_sources_behavior(&selected);
    if let Some(source) = helper_source {
        copy_top_field(source, &mut result, "policyHelper");
        copy_top_field(source, &mut result, "policyHelpers");
    }
    result
}

fn strip_policy_helper_fields(source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut result = source.clone();
    let _ = result.remove_field("policyHelper");
    let _ = result.remove_field("policyHelpers");
    result
}

fn managed_file_source_paths(documents: &[NativePolicyDocument]) -> Vec<PathBuf> {
    documents
        .iter()
        .filter(|document| {
            document.source == NativePolicySource::ManagedFile
                && document.controls().is_some()
                && document.normalized_projection().is_some()
        })
        .flat_map(NativePolicyDocument::source_paths)
        .collect()
}

fn managed_file_unresolved_fields(documents: &[NativePolicyDocument]) -> Vec<String> {
    let mut fields = Vec::new();
    for document in documents.iter().filter(|document| {
        document.source == NativePolicySource::ManagedFile
            && document.controls().is_some()
            && document.normalized_projection().is_some()
    }) {
        for field in document.unresolved_fields() {
            if !fields.iter().any(|existing| existing == field) {
                fields.push(field.clone());
            }
        }
    }
    fields
}

fn managed_file_startup_fatal_paths(documents: &[NativePolicyDocument]) -> Vec<String> {
    let mut fields = Vec::new();
    for document in documents.iter().filter(|document| {
        document.source == NativePolicySource::ManagedFile
            && document.controls().is_some()
            && document.normalized_projection().is_some()
    }) {
        for field in document.startup_fatal_paths() {
            if !fields.iter().any(|existing| existing == field) {
                fields.push(field.clone());
            }
        }
    }
    fields
}

fn collect_startup_fatal_helper_paths(
    documents: &[NativePolicyDocument],
) -> Vec<NativePolicyStartupFatal> {
    documents
        .iter()
        .filter(|document| document.can_supply_admin_policy_helpers())
        .flat_map(|document| {
            document
                .startup_fatal_paths()
                .iter()
                .map(|path| NativePolicyStartupFatal {
                    source: document.source,
                    source_paths: document.source_paths(),
                    path: path.clone(),
                    diagnostic: document
                        .startup_fatal_diagnostics()
                        .iter()
                        .find(|diagnostic| diagnostic.path == *path)
                        .and_then(|diagnostic| diagnostic.message.clone()),
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn native_policy_helper_plan(
    candidate: &AdminCandidate,
    platform: NativePolicyPlatform,
) -> Option<NativePolicyHelperPlan> {
    // Native documents `policyHelper` as admin-controlled only. A user-
    // writable plist candidate may still contribute ordinary policy settings,
    // but it cannot authorize helper execution or startup-fatal helper errors.
    if candidate.source_user_writable {
        return None;
    }
    let policy_helper = value_at_path(&candidate.projection, &["policyHelper"]);
    let policy_helpers = value_at_path(&candidate.projection, &["policyHelpers"]);
    let unresolved_helper_fields = candidate
        .unresolved_fields
        .iter()
        .filter(|field| field.starts_with("policyHelper"))
        .cloned()
        .collect::<Vec<_>>();
    if policy_helper.is_none()
        && policy_helpers.is_none()
        && unresolved_helper_fields.is_empty()
        && candidate.startup_fatal_paths.is_empty()
    {
        return None;
    }

    let platform_chain = platform.ordered_policy_keys();
    let selected_entry = policy_helpers
        .as_ref()
        .and_then(|map| select_native_helper_entry(map, &platform_chain));
    let static_fallback_candidates = policy_helpers
        .as_ref()
        .map(|map| collect_native_static_fallback_candidates(map, &platform_chain))
        .unwrap_or_default();
    let singular_entry = policy_helper
        .as_ref()
        .and_then(|config| helper_entry_from_config(config, "policyHelper", false));
    let selected_static_fallback_field = static_fallback_candidates
        .iter()
        .find(|candidate| candidate.on_chain)
        .map(|candidate| candidate.source_field.clone());
    let selected_kind = if selected_entry.is_some() {
        NativePolicyHelperSelectedKind::PerOsHelper
    } else if selected_static_fallback_field.is_some() {
        NativePolicyHelperSelectedKind::StaticFallback
    } else if singular_entry.is_some() {
        NativePolicyHelperSelectedKind::SingularHelper
    } else {
        NativePolicyHelperSelectedKind::NoEntry
    };
    let validation = if unresolved_helper_fields.is_empty() {
        NativePolicyHelperPlanValidation::Validated
    } else {
        NativePolicyHelperPlanValidation::Unresolved {
            fields: unresolved_helper_fields,
        }
    };
    Some(NativePolicyHelperPlan {
        source: candidate.source,
        source_paths: candidate.source_paths.clone(),
        source_user_writable: candidate.source_user_writable,
        platform,
        platform_chain: platform_chain.into_iter().map(str::to_owned).collect(),
        policy_helper,
        policy_helpers,
        selected_entry,
        singular_entry,
        selected_kind,
        selected_static_fallback_field,
        validation,
        startup_fatal_paths: candidate.startup_fatal_paths.clone(),
        static_fallback_candidates,
    })
}

fn select_native_helper_entry(
    map: &Utf16JsonProjection,
    platform_chain: &[&str],
) -> Option<NativePolicyHelperEntry> {
    for entry_key in platform_chain {
        let Some(config) = value_at_path(map, &[*entry_key]) else {
            continue;
        };
        let Some(root) = config.at_pointer("") else {
            continue;
        };
        if root.object_entries().is_none() {
            continue;
        }
        let has_path = config
            .at_pointer("/path")
            .and_then(JsonValueView::code_units)
            .is_some_and(|path| !path.is_empty());
        let has_inline = config
            .at_pointer("/script")
            .and_then(JsonValueView::code_units)
            .is_some_and(|script| !script.is_empty())
            && config
                .string_at("/interpreter")
                .is_some_and(|interpreter| matches!(interpreter, "sh" | "pwsh"));
        if !has_path && !has_inline {
            continue;
        }
        return helper_entry_from_config(&config, entry_key, true);
    }
    None
}

fn helper_entry_from_config(
    config: &Utf16JsonProjection,
    entry_key: &str,
    from_per_os: bool,
) -> Option<NativePolicyHelperEntry> {
    if config
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .is_none()
    {
        return None;
    }

    let mut normalized_config = config.clone();
    let _ = normalized_config.remove_field("defaultSettings");
    let on_failure = match normalized_config.string_at("/onFailure") {
        Some("continue") => NativePolicyHelperOnFailure::Continue,
        // Local helpers default to refusal. Invalid values are also normalized
        // to `refuse` by the source parser before this projection is consumed.
        _ => NativePolicyHelperOnFailure::Refuse,
    };
    let output_behavior = match normalized_config.string_at("/outputBehavior") {
        Some("merge") => NativePolicyHelperOutputBehavior::Merge,
        _ => NativePolicyHelperOutputBehavior::Replace,
    };

    Some(NativePolicyHelperEntry {
        entry_key: entry_key.to_owned(),
        config: normalized_config,
        from_per_os,
        on_failure,
        timeout_ms: helper_config_u32(config, "timeoutMs", 10_000),
        retries: helper_config_u8(config, "retries", 0),
        refresh_interval_ms: helper_config_u32(config, "refreshIntervalMs", 0),
        output_behavior,
    })
}

fn helper_config_u32(config: &Utf16JsonProjection, field: &str, default: u32) -> u32 {
    config
        .at_pointer(&format!("/{field}"))
        .and_then(JsonValueView::as_number)
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| number.min(f64::from(u32::MAX)) as u32)
        .unwrap_or(default)
}

fn helper_config_u8(config: &Utf16JsonProjection, field: &str, default: u8) -> u8 {
    config
        .at_pointer(&format!("/{field}"))
        .and_then(JsonValueView::as_number)
        .filter(|number| number.is_finite() && *number >= 0.0)
        .map(|number| number.min(f64::from(u8::MAX)) as u8)
        .unwrap_or(default)
}

fn collect_native_static_fallback_candidates(
    map: &Utf16JsonProjection,
    platform_chain: &[&str],
) -> Vec<NativePolicyStaticFallbackCandidate> {
    let mut candidates = Vec::new();
    let empty_semantics = empty_native_policy_semantics();
    for entry_key in platform_chain {
        if let Some(settings) = value_at_path(map, &[*entry_key, "defaultSettings"]) {
            if settings.at_pointer("").is_none_or(JsonValueView::is_null) {
                continue;
            }
            let source_field = format!("policyHelpers.{entry_key}.defaultSettings");
            let merges_output =
                map.string_at(&format!("/{entry_key}/outputBehavior")) == Some("merge");
            candidates.push(validate_static_fallback_candidate(
                source_field,
                settings,
                true,
                merges_output,
                &empty_semantics,
            ));
        }
    }
    const PLATFORM_KEYS: [&str; 4] = ["macos", "linux", "windows", "wsl"];
    for entry_key in PLATFORM_KEYS {
        if platform_chain.contains(&entry_key) {
            continue;
        }
        if let Some(settings) = value_at_path(map, &[entry_key, "defaultSettings"]) {
            if settings.at_pointer("").is_none_or(JsonValueView::is_null) {
                continue;
            }
            let source_field = format!("policyHelpers.{entry_key}.defaultSettings");
            candidates.push(validate_static_fallback_candidate(
                source_field,
                settings,
                false,
                false,
                &empty_semantics,
            ));
        }
    }
    if let Some(settings) = value_at_path(map, &["default"]) {
        if settings.at_pointer("").is_some_and(JsonValueView::is_null) {
            return candidates;
        }
        candidates.push(validate_static_fallback_candidate(
            "policyHelpers.default".into(),
            settings,
            true,
            false,
            &empty_semantics,
        ));
    }
    candidates
}

fn empty_native_policy_semantics() -> NativePolicySemanticEvidence {
    NativePolicySemanticEvidence {
        regex_rows: Vec::new(),
        js: NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
            .expect("empty Native policy semantic registry is valid"),
    }
}

fn validate_static_fallback_candidate(
    source_field: String,
    settings: Utf16JsonProjection,
    on_chain: bool,
    merges_output: bool,
    semantics: &NativePolicySemanticEvidence,
) -> NativePolicyStaticFallbackCandidate {
    let semantic_inputs =
        collect_native_policy_semantic_inputs_with_js(&settings, Some(&semantics.js));
    let result = validate_native_static_default_settings(&settings, &source_field, semantics);
    let diagnostic = native_static_settings_diagnostic(&result, &source_field)
        .map(|diagnostic| diagnostic.message);
    let validation = match result.status {
        NativeStaticSettingsStatus::Valid if result.normalized.is_some() => {
            if semantics.covers(&semantic_inputs) {
                NativePolicyStaticFallbackValidation::Validated
            } else {
                NativePolicyStaticFallbackValidation::Unresolved {
                    fields: semantic_inputs_uncovered(&semantic_inputs, semantics, &source_field),
                }
            }
        }
        NativeStaticSettingsStatus::Invalid if diagnostic.is_some() => {
            NativePolicyStaticFallbackValidation::Rejected
        }
        NativeStaticSettingsStatus::Valid
        | NativeStaticSettingsStatus::Invalid
        | NativeStaticSettingsStatus::Unresolved => {
            let fields = if result.unresolved_fields.is_empty() {
                vec![source_field.clone()]
            } else {
                result.unresolved_fields.clone()
            };
            NativePolicyStaticFallbackValidation::Unresolved { fields }
        }
    };
    NativePolicyStaticFallbackCandidate {
        source_field,
        settings,
        normalized_settings: result.normalized,
        diagnostic,
        warnings: result.static_fallback_warnings,
        warning_unresolved_fields: result.warning_unresolved_fields,
        regex_patterns: result.regex_patterns,
        semantic_inputs,
        on_chain,
        merges_output,
        validation,
    }
}

fn push_unique_string(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

fn semantic_inputs_uncovered(
    inputs: &NativePolicySemanticInputs,
    evidence: &NativePolicySemanticEvidence,
    source_field: &str,
) -> Vec<String> {
    let mut fields = Vec::new();
    for pattern in &inputs.regex_patterns {
        if !evidence
            .regex_rows
            .iter()
            .any(|row| row.pattern_units.as_slice() == pattern.as_slice())
        {
            push_unique_string(&mut fields, format!("{source_field}.<regex-worker-result>"));
            break;
        }
    }
    if inputs
        .url_inputs
        .iter()
        .any(|input| evidence.js.url(input).is_none())
    {
        push_unique_string(&mut fields, format!("{source_field}.<url-worker-result>"));
    }
    if inputs
        .display_inputs
        .iter()
        .any(|input| evidence.js.display(input).is_none())
    {
        push_unique_string(
            &mut fields,
            format!("{source_field}.<display-worker-result>"),
        );
    }
    fields
}

fn strip_managed_sources_behavior(source: &Utf16JsonProjection) -> Utf16JsonProjection {
    let mut result = source.clone();
    let _ = result.remove_field("managedSourcesBehavior");
    result
}

fn native_merge(
    low_priority: &Utf16JsonProjection,
    high_priority: &Utf16JsonProjection,
) -> Utf16JsonProjection {
    let (Some(low_entries), Some(high_entries)) = (
        low_priority.object_entries(),
        high_priority.object_entries(),
    ) else {
        return high_priority.clone();
    };
    let mut fields = low_entries
        .into_iter()
        .map(|entry| {
            (
                entry.code_units.to_vec(),
                low_priority
                    .subprojection_property_units(entry.code_units)
                    .expect("object entry has a value"),
            )
        })
        .collect::<Vec<_>>();
    for entry in high_entries {
        let value = high_priority
            .subprojection_property_units(entry.code_units)
            .expect("object entry has a value");
        let Some(index) = fields
            .iter()
            .position(|(key, _)| key.as_slice() == entry.code_units)
        else {
            fields.push((entry.code_units.to_vec(), value));
            continue;
        };
        let current = &fields[index].1;
        let merged = match (current.object_entries(), value.object_entries()) {
            (Some(_), Some(_)) => native_merge(current, &value),
            _ if current
                .at_pointer("")
                .and_then(JsonValueView::array_len)
                .is_some()
                && value
                    .at_pointer("")
                    .and_then(JsonValueView::array_len)
                    .is_some() =>
            {
                merge_priority_arrays(current, &value)
            }
            _ => value,
        };
        fields[index].1 = merged;
    }
    Utf16JsonProjection::object_units(fields).unwrap_or_else(|_| high_priority.clone())
}

fn merge_priority_arrays(
    low_priority: &Utf16JsonProjection,
    high_priority: &Utf16JsonProjection,
) -> Utf16JsonProjection {
    let Some(high_view) = high_priority.at_pointer("") else {
        return high_priority.clone();
    };
    let Some(low_view) = low_priority.at_pointer("") else {
        return high_priority.clone();
    };
    let mut values = Vec::new();
    for (projection, view) in [(high_priority, high_view), (low_priority, low_view)] {
        let Some(length) = view.array_len() else {
            continue;
        };
        for index in 0..length {
            let Ok(value) = projection.subprojection(&format!("/{index}")) else {
                continue;
            };
            let is_object = value
                .at_pointer("")
                .is_some_and(|item| item.object_entries().is_some() || item.array_len().is_some());
            if is_object
                || !values
                    .iter()
                    .any(|existing: &Utf16JsonProjection| existing.semantically_eq(&value))
            {
                values.push(value);
            }
        }
    }
    Utf16JsonProjection::array(values).unwrap_or_else(|_| high_priority.clone())
}

#[derive(Debug, Clone, Copy)]
enum RestrictionAtom {
    Bool(bool),
    String(&'static str),
}

struct RestrictionSpec {
    path: &'static [&'static str],
    restrictive: &'static [RestrictionAtom],
}

macro_rules! restriction {
    ([$($path:literal),+ $(,)?], [$($value:tt),+ $(,)?]) => {
        RestrictionSpec {
            path: &[$($path),+],
            restrictive: &[$(restriction!(@atom $value)),+],
        }
    };
    (@atom true) => { RestrictionAtom::Bool(true) };
    (@atom false) => { RestrictionAtom::Bool(false) };
    (@atom $value:literal) => { RestrictionAtom::String($value) };
}

const RESTRICTIONS: &[RestrictionSpec] = &[
    restriction!(["allowManagedPermissionRulesOnly"], [true]),
    restriction!(["allowManagedHooksOnly"], [true]),
    restriction!(["allowManagedMcpServersOnly"], [true]),
    restriction!(["enforceAvailableModels"], [true]),
    restriction!(["disableAllHooks"], [true]),
    restriction!(["disableClaudeAiConnectors"], [true]),
    restriction!(["disableCommandPluginSources"], [true]),
    restriction!(["disableSideloadFlags"], [true]),
    restriction!(["disableSkillShellExecution"], [true]),
    restriction!(["disableRemoteControl"], [true]),
    restriction!(["disableAgentView"], [true]),
    restriction!(["disableWorkflows"], [true]),
    restriction!(["disableArtifact"], [true]),
    restriction!(["disableBundledSkills"], [true]),
    restriction!(["fastModePerSessionOptIn"], [true]),
    restriction!(["isolatePeerMachines"], [true]),
    restriction!(["strictPluginOnlyCustomization"], [true]),
    restriction!(["disableAutoMode"], ["disable"]),
    restriction!(["disableDeepLinkRegistration"], ["disable"]),
    restriction!(["permissions", "disableBypassPermissionsMode"], ["disable"]),
    restriction!(["permissions", "disableAutoMode"], ["disable"]),
    restriction!(
        ["permissions", "blockReadsOutsideWorkingDirectories"],
        [true]
    ),
    restriction!(["autoMode", "classifyAllShell"], [true]),
    restriction!(["worktree", "bgIsolation"], ["worktree"]),
    restriction!(["enableArtifact"], [false]),
    restriction!(["enableWorkflows"], [false]),
    restriction!(["syncClaudeAiSkills"], [false]),
    restriction!(["syncClaudeAiPlugins"], [false]),
    restriction!(["useAutoModeDuringPlan"], [false]),
    restriction!(["skipDangerousModePermissionPrompt"], [false]),
    restriction!(["skipAutoPermissionPrompt"], [false]),
    restriction!(["enableAllProjectMcpServers"], [false]),
    restriction!(["channelsEnabled"], [false]),
    restriction!(["skipWebFetchPreflight"], [false]),
    restriction!(["skipWorkflowUsageWarning"], [false]),
    restriction!(["autoUploadSessions"], [false]),
    restriction!(["remoteControlAtStartup"], [false]),
    restriction!(["remoteTools", "allowUnattendedServing"], [false]),
    restriction!(["autoContinueAtUsageLimit"], [false]),
    restriction!(["disablePluginDirectory"], [true]),
    restriction!(["attribution", "sessionUrl"], [false]),
    restriction!(["crossSessionInbound"], ["refuse", "hold"]),
    restriction!(["remoteControl", "shareHostProfile"], ["off", "basic"]),
    restriction!(["modelProposedGoals"], ["disabled", "alwaysAsk"]),
    restriction!(["feedbackDrafts"], ["off"]),
    restriction!(["availableModelsMatch"], ["exact"]),
    restriction!(["askUserQuestionTimeout"], ["never"]),
    restriction!(["dialogExpiry"], ["never"]),
    restriction!(["sandbox", "enabled"], [true]),
    restriction!(["sandbox", "failIfUnavailable"], [true]),
    restriction!(["sandbox", "autoAllowBashIfSandboxed"], [false]),
    restriction!(["sandbox", "allowUnsandboxedCommands"], [false]),
    restriction!(["sandbox", "enableWeakerNestedSandbox"], [false]),
    restriction!(["sandbox", "enableWeakerNetworkIsolation"], [false]),
    restriction!(["sandbox", "allowAppleEvents"], [false]),
    restriction!(["sandbox", "network", "allowManagedDomainsOnly"], [true]),
    restriction!(["sandbox", "network", "strictAllowlist"], [true]),
    restriction!(["sandbox", "network", "allowAllUnixSockets"], [false]),
    restriction!(["sandbox", "network", "allowLocalBinding"], [false]),
    restriction!(
        ["sandbox", "filesystem", "allowManagedReadPathsOnly"],
        [true]
    ),
    restriction!(["sandbox", "filesystem", "disabled"], [false]),
    restriction!(["sandbox", "credentials", "allowPlaintextInject"], [false]),
    restriction!(["sandbox", "credentials", "sigv4", "streaming"], ["deny"]),
    restriction!(["sandbox", "credentials", "sigv4", "presigned"], ["deny"]),
    restriction!(["sandbox", "credentials", "sigv4", "sigv4a"], ["deny"]),
    restriction!(["isolation", "required"], [true]),
    restriction!(["isolation", "persistHome"], [false]),
];

fn apply_admin_restrictions(effective: &mut Utf16JsonProjection, tiers: &[AdminCandidate]) {
    let tier_projections = tiers
        .iter()
        .map(|tier| &tier.projection)
        .collect::<Vec<_>>();
    for spec in RESTRICTIONS {
        let Some(first_value) = tier_projections
            .first()
            .and_then(|tier| value_at_path(tier, spec.path))
        else {
            continue;
        };
        let mut selected: Option<Utf16JsonProjection> = None;
        let mut selected_rank = usize::MAX;
        for tier in &tier_projections {
            let Some(value) = value_at_path(tier, spec.path) else {
                continue;
            };
            if let Some(rank) = spec
                .restrictive
                .iter()
                .position(|atom| atom_matches(*atom, &value))
            {
                if rank < selected_rank {
                    selected_rank = rank;
                    selected = Some(value);
                }
            }
        }
        if let Some(value) = selected {
            let _ = set_value_at_path(effective, spec.path, value);
        } else if value_at_path(&tier_projections[0], spec.path).is_none() {
            let _ = remove_value_at_path(effective, spec.path);
        } else if first_value
            .at_pointer("")
            .is_some_and(JsonValueView::is_null)
        {
            // Native keeps an explicit top-source null unless a recognized
            // restrictive value appears in another tier.
            let _ = set_value_at_path(effective, spec.path, first_value);
        }
    }

    let effective_strict_only = value_at_path(effective, &["strictPluginOnlyCustomization"])
        .is_some_and(|value| projection_is_bool(&value, true));
    if !effective_strict_only {
        let mut entries = Vec::new();
        for tier in &tier_projections {
            if let Some(value) = value_at_path(tier, &["strictPluginOnlyCustomization"]) {
                if let Some(items) = projection_array_items(&value) {
                    entries.extend(items);
                }
            }
        }
        let entries = dedup_primitives(entries);
        if !entries.is_empty() {
            if let Ok(array) = Utf16JsonProjection::array(entries) {
                let _ = effective.set_field("strictPluginOnlyCustomization", array);
            }
        } else if value_at_path(&tier_projections[0], &["strictPluginOnlyCustomization"]).is_none()
        {
            let _ = effective.remove_field("strictPluginOnlyCustomization");
        }
    }
}

fn parent_restrictive_slice(
    parent: &Utf16JsonProjection,
    admin: Option<&Utf16JsonProjection>,
) -> Utf16JsonProjection {
    let mut result = empty_object();
    for (key, required) in [
        ("allowManagedHooksOnly", true),
        ("disableCommandPluginSources", true),
        ("allowManagedMcpServersOnly", true),
        ("disableClaudeAiConnectors", true),
        ("syncClaudeAiSkills", false),
        ("syncClaudeAiPlugins", false),
        ("allowManagedPermissionRulesOnly", true),
        ("enforceAvailableModels", true),
    ] {
        if let Some(value) = parent.get_property(key) {
            if value.as_bool() == Some(required) {
                copy_top_field(parent, &mut result, key);
            }
        }
    }
    if parent.string_at("/disableAutoMode") == Some("disable") {
        copy_top_field(parent, &mut result, "disableAutoMode");
    }
    if let Some(value) = parent.get_property("disablePluginDirectory") {
        if value.as_bool() == Some(true) {
            copy_top_field(parent, &mut result, "disablePluginDirectory");
        }
    }
    if let Some(value) = parent.at_pointer("/remoteTools/allowUnattendedServing") {
        if value.as_bool() == Some(false) {
            copy_path(
                parent,
                &mut result,
                &["remoteTools", "allowUnattendedServing"],
            );
        }
    }
    if let Some(value) = parent.at_pointer("/remoteControl/shareHostProfile") {
        if value
            .as_str()
            .is_some_and(|value| value == "off" || value == "basic")
        {
            copy_path(parent, &mut result, &["remoteControl", "shareHostProfile"]);
        }
    }
    if let Some(value) = parent.at_pointer("/attribution/sessionUrl") {
        if value.as_bool() == Some(false) {
            copy_path(parent, &mut result, &["attribution", "sessionUrl"]);
        }
    }
    if let Some(value) = parent.at_pointer("/attribution/commitTrailers") {
        if value.as_bool() == Some(false) {
            copy_path(parent, &mut result, &["attribution", "commitTrailers"]);
        }
    } else if parent
        .get_property("includeCoAuthoredBy")
        .and_then(JsonValueView::as_bool)
        == Some(false)
    {
        let _ = set_value_at_path(
            &mut result,
            &["attribution", "commitTrailers"],
            bool_projection(false),
        );
    }
    if let Some(value) = parent.get_property("strictPluginOnlyCustomization") {
        if value.as_bool() == Some(true) || value.array_len().is_some_and(|length| length > 0) {
            copy_top_field(parent, &mut result, "strictPluginOnlyCustomization");
        }
    }
    for (key, require_nonempty) in [
        ("deniedMcpServers", false),
        ("blockedMarketplaces", true),
        ("deniedModels", true),
    ] {
        if let Some(value) = parent.get_property(key) {
            let nonempty = value.array_len().is_some_and(|length| length > 0)
                || value
                    .object_entries()
                    .is_some_and(|entries| !entries.is_empty());
            if !require_nonempty || nonempty {
                copy_top_field(parent, &mut result, key);
            }
        }
    }
    if admin
        .and_then(|value| value.get_property("forceLoginOrgUUID"))
        .is_none()
    {
        if let Some(value) = parent.get_property("forceLoginOrgUUID") {
            if !value.is_null() {
                copy_top_field(parent, &mut result, "forceLoginOrgUUID");
            }
        }
    }
    for key in [
        "allowedMcpServers",
        "availableModels",
        "strictKnownMarketplaces",
        "allowedProviders",
    ] {
        if admin.and_then(|value| value.get_property(key)).is_none()
            && parent.get_property(key).is_some()
        {
            copy_top_field(parent, &mut result, key);
        }
    }
    filter_parent_permissions(parent, admin, &mut result);
    filter_parent_sandbox(parent, admin, &mut result);
    apply_parent_sandbox_restrictions(parent, &mut result);
    result
}

fn filter_parent_permissions(
    parent: &Utf16JsonProjection,
    admin: Option<&Utf16JsonProjection>,
    result: &mut Utf16JsonProjection,
) {
    let Some(permissions) = parent.subprojection("/permissions").ok() else {
        return;
    };
    let mut selected = empty_object();
    for key in ["deny", "ask"] {
        if let Some(items) = value_at_path(&permissions, &[key]) {
            if let Some(filtered) = filter_exception_rules(&items) {
                let _ = selected.set_field(key, filtered);
            }
        }
    }
    for key in ["disableBypassPermissionsMode", "disableAutoMode"] {
        if permissions.string_at(&format!("/{key}")) == Some("disable") {
            copy_top_field(&permissions, &mut selected, key);
        }
    }
    if permissions
        .get_property("blockReadsOutsideWorkingDirectories")
        .and_then(JsonValueView::as_bool)
        == Some(true)
    {
        copy_top_field(
            &permissions,
            &mut selected,
            "blockReadsOutsideWorkingDirectories",
        );
    }
    let permission_rules_only = admin
        .and_then(|admin| admin.get_property("allowManagedPermissionRulesOnly"))
        .and_then(JsonValueView::as_bool)
        == Some(true);
    let managed_domains_only = admin
        .and_then(|admin| admin.at_pointer("/sandbox/network/allowManagedDomainsOnly"))
        .and_then(JsonValueView::as_bool)
        == Some(true);
    if !permission_rules_only && !managed_domains_only {
        for key in ["allow", "additionalDirectories"] {
            if let Some(value) = value_at_path(&permissions, &[key]) {
                let _ = selected.set_field(key, value);
            }
        }
    }
    if !is_empty_object(&selected) {
        let _ = result.set_field("permissions", selected);
    }
}

fn filter_exception_rules(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let items = projection_array_items(value)?;
    let retained = items
        .into_iter()
        .filter(|item| {
            let Some(units) = item.string_units("") else {
                return true;
            };
            units.first() != Some(&u16::from(b'!'))
        })
        .collect::<Vec<_>>();
    Utf16JsonProjection::array(retained).ok()
}

fn filter_parent_sandbox(
    parent: &Utf16JsonProjection,
    admin: Option<&Utf16JsonProjection>,
    result: &mut Utf16JsonProjection,
) {
    let Some(sandbox) = parent.subprojection("/sandbox").ok() else {
        return;
    };
    let mut selected = empty_object();
    let allow_managed_domains_only = admin
        .and_then(|admin| admin.at_pointer("/sandbox/network/allowManagedDomainsOnly"))
        .and_then(JsonValueView::as_bool)
        == Some(true);
    let allow_managed_read_paths_only = admin
        .and_then(|admin| admin.at_pointer("/sandbox/filesystem/allowManagedReadPathsOnly"))
        .and_then(JsonValueView::as_bool)
        == Some(true);

    let mut network = empty_object();
    if let Some(value) = value_at_path(&sandbox, &["network", "deniedDomains"]) {
        let _ = network.set_field("deniedDomains", value);
    }
    if !allow_managed_domains_only {
        if let Some(value) = value_at_path(&sandbox, &["network", "allowedDomains"]) {
            let _ = network.set_field("allowedDomains", value);
        }
    }
    if !is_empty_object(&network) {
        let _ = selected.set_field("network", network);
    }

    let mut filesystem = empty_object();
    for key in ["denyRead", "denyWrite"] {
        if let Some(value) = value_at_path(&sandbox, &["filesystem", key]) {
            let _ = filesystem.set_field(key, value);
        }
    }
    if !allow_managed_read_paths_only {
        if let Some(value) = value_at_path(&sandbox, &["filesystem", "allowRead"]) {
            let _ = filesystem.set_field("allowRead", value);
        }
    }
    if !is_empty_object(&filesystem) {
        let _ = selected.set_field("filesystem", filesystem);
    }

    let credentials = parent_restrictive_credentials(&sandbox);
    if !is_empty_object(&credentials) {
        let _ = selected.set_field("credentials", credentials);
    }
    if !is_empty_object(&selected) {
        let _ = result.set_field("sandbox", selected);
    }
}

fn parent_restrictive_credentials(sandbox: &Utf16JsonProjection) -> Utf16JsonProjection {
    let Some(credentials) = sandbox.subprojection("/credentials").ok() else {
        return empty_object();
    };
    let mut result = empty_object();
    if let Some(items) =
        value_at_path(&credentials, &["files"]).and_then(|value| projection_array_items(&value))
    {
        let files = items
            .into_iter()
            .filter_map(|item| {
                let path = item.get_property("path")?;
                let mode = item.get_property("mode")?;
                let path_projection = item.subprojection("/path").ok()?;
                let mode_value = if mode.as_str() == Some("deny") {
                    "deny"
                } else {
                    "mask"
                };
                let mut entry = empty_object();
                let _ = entry.set_field("path", path_projection);
                let _ = entry.set_field("mode", string_projection(mode_value));
                if mode_value == "mask" {
                    let _ = entry.set_field("injectHosts", empty_array());
                }
                let _ = path;
                Some(entry)
            })
            .collect::<Vec<_>>();
        if let Ok(files) = Utf16JsonProjection::array(files) {
            if !projection_array_is_empty(&files) {
                let _ = result.set_field("files", files);
            }
        }
    }
    if let Some(items) =
        value_at_path(&credentials, &["envVars"]).and_then(|value| projection_array_items(&value))
    {
        let env_vars = items
            .into_iter()
            .filter(|item| item.string_at("/mode") == Some("deny"))
            .filter_map(|item| {
                let name = item.subprojection("/name").ok()?;
                let mut entry = empty_object();
                let _ = entry.set_field("name", name);
                let _ = entry.set_field("mode", string_projection("deny"));
                Some(entry)
            })
            .collect::<Vec<_>>();
        if let Ok(env_vars) = Utf16JsonProjection::array(env_vars) {
            if !projection_array_is_empty(&env_vars) {
                let _ = result.set_field("envVars", env_vars);
            }
        }
    }
    if let Some(sigv4) = value_at_path(&credentials, &["sigv4"]) {
        if let Some(entries) = sigv4.object_entries() {
            let denied = entries
                .into_iter()
                .filter(|entry| entry.value.as_str() == Some("deny"))
                .filter_map(|entry| {
                    sigv4
                        .subprojection_property_units(entry.code_units)
                        .ok()
                        .map(|value| (entry.code_units.to_vec(), value))
                })
                .collect::<Vec<_>>();
            if let Ok(denied) = Utf16JsonProjection::object_units(denied) {
                if !is_empty_object(&denied) {
                    let _ = result.set_field("sigv4", denied);
                }
            }
        }
    }
    if let Some(value) = value_at_path(&credentials, &["awsPairs"]) {
        if !projection_array_is_empty(&value) {
            let _ = result.set_field("awsPairs", value);
        }
    }
    result
}

fn apply_parent_sandbox_restrictions(
    parent: &Utf16JsonProjection,
    result: &mut Utf16JsonProjection,
) {
    let mut sandbox = result
        .subprojection("/sandbox")
        .unwrap_or_else(|_| empty_object());
    let mut changed = false;
    for spec in RESTRICTIONS
        .iter()
        .filter(|spec| spec.path.first() == Some(&"sandbox"))
    {
        if let Some(value) = value_at_path(parent, spec.path) {
            if spec
                .restrictive
                .iter()
                .any(|atom| atom_matches(*atom, &value))
            {
                let nested = &spec.path[1..];
                if !nested.is_empty() && set_value_at_path(&mut sandbox, nested, value).is_ok() {
                    changed = true;
                }
            }
        }
    }
    if changed {
        let _ = result.set_field("sandbox", sandbox);
    }
}

fn parse_typed_settings(projection: &Utf16JsonProjection) -> Option<SettingsJson> {
    let json = projection.to_json_string().ok()?;
    let settings = serde_json::from_str::<SettingsJson>(&json).ok()?;
    settings.validate().ok()?;
    Some(settings)
}

fn strip_parent_policy_controls(projection: &mut Option<Utf16JsonProjection>) {
    if let Some(projection) = projection {
        // Native k0t() removes administrator-source controls from the parent
        // slot before applying the restrictive parent allowlist.
        let _ = projection.remove_field("managedSourcesBehavior");
        let _ = projection.remove_field("wslInheritsWindowsSettings");
    }
}

fn parse_controls(projection: &Utf16JsonProjection) -> Result<NativePolicyControls, String> {
    let managed_sources_behavior = match enum_field(
        projection,
        "managedSourcesBehavior",
        &["first-wins", "merge"],
    )? {
        Some("merge") => NativeManagedSourcesBehavior::Merge,
        Some("first-wins") | None => NativeManagedSourcesBehavior::FirstWins,
        Some(_) => unreachable!("enum_field validates its allowed values"),
    };
    let parent_settings_behavior = match enum_field(
        projection,
        "parentSettingsBehavior",
        &["first-wins", "merge"],
    )? {
        Some("merge") => NativeParentSettingsBehavior::Merge,
        Some("first-wins") | None => NativeParentSettingsBehavior::FirstWins,
        Some(_) => unreachable!("enum_field validates its allowed values"),
    };
    Ok(NativePolicyControls {
        managed_sources_behavior,
        parent_settings_behavior,
    })
}

fn enum_field(
    projection: &Utf16JsonProjection,
    key: &str,
    allowed: &[&'static str],
) -> Result<Option<&'static str>, String> {
    let Some(value) = projection.get_property(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Err(format!("{key} is optional but does not accept null"));
    }
    let Some(units) = value.code_units() else {
        return Err(format!("{key} must be one of {}", allowed.join(", ")));
    };
    let Ok(value) = String::from_utf16(units) else {
        return Err(format!("{key} must be valid UTF-16 text"));
    };
    allowed
        .iter()
        .copied()
        .find(|candidate| *candidate == value)
        .map(Some)
        .ok_or_else(|| format!("{key} has unsupported value {value:?}"))
}

fn unresolved_document(document: &NativePolicyDocument) -> Vec<NativePolicyUnresolved> {
    match &document.admission {
        NativePolicyAdmission::NativePolicyAccepted { .. } => document
            .unresolved_fields
            .iter()
            .map(|field| NativePolicyUnresolved {
                source: document.source,
                reason: format!("Native policy field parser is unresolved at {field}"),
            })
            .collect(),
        NativePolicyAdmission::NativePolicyRejected { reason } => {
            vec![NativePolicyUnresolved {
                source: document.source,
                reason: reason.clone(),
            }]
        }
        NativePolicyAdmission::Unavailable {
            load_state: NativePolicyLoadState::Absent,
        } => Vec::new(),
        NativePolicyAdmission::Unavailable { load_state } => {
            vec![NativePolicyUnresolved {
                source: document.source,
                reason: format!("source did not provide a projection ({load_state:?})"),
            }]
        }
    }
}

fn native_has_policy_content(projection: &Utf16JsonProjection) -> bool {
    projection.object_entries().is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| !is_policy_control_key(entry.code_units))
    })
}

fn native_has_nonempty_policy_content(projection: &Utf16JsonProjection) -> bool {
    projection.object_entries().is_some_and(|entries| {
        entries.iter().any(|entry| {
            if is_policy_control_key(entry.code_units) {
                return false;
            }
            projection
                .subprojection_property_units(entry.code_units)
                .is_ok_and(|value| !is_deep_empty(&value))
        })
    })
}

fn is_deep_empty(projection: &Utf16JsonProjection) -> bool {
    let Some(root) = projection.at_pointer("") else {
        return true;
    };
    if let Some(entries) = root.object_entries() {
        return entries.iter().all(|entry| {
            projection
                .subprojection_property_units(entry.code_units)
                .is_ok_and(|value| is_deep_empty(&value))
        });
    }
    if let Some(length) = root.array_len() {
        return (0..length).all(|index| {
            projection
                .subprojection(&format!("/{index}"))
                .is_ok_and(|value| is_deep_empty(&value))
        });
    }
    false
}

fn value_at_path(projection: &Utf16JsonProjection, path: &[&str]) -> Option<Utf16JsonProjection> {
    let pointer = path
        .iter()
        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
        .collect::<String>();
    projection.subprojection(&pointer).ok()
}

fn is_policy_control_key(units: &[u16]) -> bool {
    units == "managedSourcesBehavior".encode_utf16().collect::<Vec<_>>()
        || units
            == "wslInheritsWindowsSettings"
                .encode_utf16()
                .collect::<Vec<_>>()
}

fn set_value_at_path(
    projection: &mut Utf16JsonProjection,
    path: &[&str],
    value: Utf16JsonProjection,
) -> Result<(), ()> {
    let Some((field, tail)) = path.split_first() else {
        return Err(());
    };
    if tail.is_empty() {
        return projection.set_field(field, value).map_err(|_| ());
    }
    let mut child = value_at_path(projection, &[*field]).unwrap_or_else(empty_object);
    set_value_at_path(&mut child, tail, value)?;
    projection.set_field(field, child).map_err(|_| ())
}

fn remove_value_at_path(projection: &mut Utf16JsonProjection, path: &[&str]) -> Result<(), ()> {
    let Some((field, tail)) = path.split_first() else {
        return Err(());
    };
    if tail.is_empty() {
        let _ = projection.remove_field(field).map_err(|_| ())?;
        return Ok(());
    }
    let Some(mut child) = value_at_path(projection, &[*field]) else {
        return Ok(());
    };
    remove_value_at_path(&mut child, tail)?;
    if is_empty_object(&child) {
        let _ = projection.remove_field(field).map_err(|_| ())?;
    } else {
        projection.set_field(field, child).map_err(|_| ())?;
    }
    Ok(())
}

fn atom_matches(atom: RestrictionAtom, value: &Utf16JsonProjection) -> bool {
    match atom {
        RestrictionAtom::Bool(expected) => {
            value.at_pointer("").and_then(JsonValueView::as_bool) == Some(expected)
        }
        RestrictionAtom::String(expected) => value.string_at("") == Some(expected),
    }
}

fn projection_is_bool(value: &Utf16JsonProjection, expected: bool) -> bool {
    value.at_pointer("").and_then(JsonValueView::as_bool) == Some(expected)
}

fn projection_array_items(value: &Utf16JsonProjection) -> Option<Vec<Utf16JsonProjection>> {
    let length = value.at_pointer("")?.array_len()?;
    Some(
        (0..length)
            .filter_map(|index| value.subprojection(&format!("/{index}")).ok())
            .collect(),
    )
}

fn projection_array_is_empty(value: &Utf16JsonProjection) -> bool {
    value
        .at_pointer("")
        .and_then(JsonValueView::array_len)
        .is_none_or(|length| length == 0)
}

fn dedup_primitives(values: Vec<Utf16JsonProjection>) -> Vec<Utf16JsonProjection> {
    let mut result = Vec::new();
    for value in values {
        let is_object = value
            .at_pointer("")
            .is_some_and(|item| item.object_entries().is_some() || item.array_len().is_some());
        if is_object
            || !result
                .iter()
                .any(|existing: &Utf16JsonProjection| existing.semantically_eq(&value))
        {
            result.push(value);
        }
    }
    result
}

fn copy_top_field(
    source: &Utf16JsonProjection,
    destination: &mut Utf16JsonProjection,
    field: &str,
) {
    if let Some(value) = value_at_path(source, &[field]) {
        let _ = destination.set_field(field, value);
    }
}

fn copy_path(source: &Utf16JsonProjection, destination: &mut Utf16JsonProjection, path: &[&str]) {
    if let Some(value) = value_at_path(source, path) {
        let _ = set_value_at_path(destination, path, value);
    }
}

fn empty_object() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("{}").expect("literal empty object is valid")
}

fn empty_array() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("[]").expect("literal empty array is valid")
}

fn string_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&serde_json::to_string(value).expect("string serialization"))
        .expect("serialized string is valid JSON")
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("literal boolean is valid")
}

fn is_empty_object(value: &Utf16JsonProjection) -> bool {
    value
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .is_some_and(|entries| entries.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plist(raw: &str, path: &str, user_writable: bool) -> NativePolicyDocument {
        NativePolicyDocument::parse_plist(
            Some(Utf16JsonProjection::parse(raw).expect("fixture JSON")),
            NativePolicyPlistFacts {
                path: PathBuf::from(path),
                user_writable,
                load_state: NativePolicyLoadState::Loaded,
                document_has_policy_content: None,
            },
        )
    }

    fn managed_file(raw: &str, path: &str) -> NativePolicyDocument {
        NativePolicyDocument::parse_managed_file(
            Some(Utf16JsonProjection::parse(raw).expect("fixture JSON")),
            NativePolicyManagedFileFacts {
                path: PathBuf::from(path),
                load_state: NativePolicyLoadState::Loaded,
                document_has_policy_content: None,
            },
        )
    }

    fn parent(raw: &str) -> NativePolicyDocument {
        NativePolicyDocument::parse_parent(
            Some(Utf16JsonProjection::parse(raw).expect("fixture JSON")),
            NativePolicyParentFacts {
                provider: "test-parent-slot".into(),
            },
        )
    }

    fn host(raw: &str) -> NativePolicyDocument {
        NativePolicyDocument::parse_host(
            Some(Utf16JsonProjection::parse(raw).expect("fixture JSON")),
            NativePolicyHostFacts {
                kind: NativePolicyHostKind::RegistryFallback,
                identity: "test-hkcu".into(),
                load_state: NativePolicyLoadState::Loaded,
            },
        )
    }

    fn valid_https_row(
        value: &str,
        hostname: &str,
    ) -> super::super::native_policy_js_semantics::NativePolicyUrlSemanticRow {
        use super::super::native_policy_js_semantics::{
            NativePolicyUrlSemanticRow, NativePolicyZodUrlOutcome,
        };
        NativePolicyUrlSemanticRow {
            input_units: value.encode_utf16().collect(),
            raw_rdt_valid: true,
            raw_archive_refine_valid: true,
            raw_archive_hostname_blocked: Some(false),
            zod_url: NativePolicyZodUrlOutcome::Valid {
                zod_value_units: value.encode_utf16().collect(),
                href_units: value.encode_utf16().collect(),
                protocol_units: "https:".encode_utf16().collect(),
                username_units: Vec::new(),
                password_units: Vec::new(),
                host_units: hostname.encode_utf16().collect(),
                hostname_units: hostname.encode_utf16().collect(),
                port_units: Vec::new(),
                pathname_units: "/".encode_utf16().collect(),
                search_units: Vec::new(),
                hash_units: Vec::new(),
                origin_units: format!("https://{hostname}").encode_utf16().collect(),
                rdt_zod_value_valid: true,
                archive_zod_refine_valid: true,
                archive_zod_hostname_blocked: Some(false),
            },
        }
    }

    #[test]
    fn semantic_input_collection_visits_only_native_url_regex_and_display_paths() {
        let source = Utf16JsonProjection::parse(
            r#"{"unrelated":{"url":"https://unrelated.example","hostPattern":"unrelated"},"hooks":{"FutureEvent":null,"PreToolUse":[{"hooks":[{"type":"http","url":"https://hook.example/"}]}]},"extraKnownMarketplaces":{"url":{"source":{"source":"url","url":"https://extra.example/"}},"npm":{"source":{"source":"npm","package":"@scope/pkg","registry":"https://registry.example/"}},"settings":{"source":{"source":"settings","plugins":[{"name":"p","source":{"source":"archive","url":"https://archive.example/"}}]}},"command":{"source":{"source":"command","command":"true","url":"https://unvisited.example/"}}},"managedMcpServers":{"mcp":{"type":"http","url":"https://mcp.example/","headers":{"x":"y"},"oauth":{"clientId":"id","authServerMetadataUrl":"https://auth.example/metadata"}},"badHeaders":{"type":"http","url":"https://bad-headers.example/","headers":{"x":1},"oauth":{"authServerMetadataUrl":"https://unvisited-auth.example/"}}},"strictKnownMarketplaces":[{"source":"url","url":"https://list.example/","headers":{"x":"y"}},{"source":"url","url":"https://unvisited-list.example/","headers":{"x":1}},{"source":"hostPattern","hostPattern":"^host$"}],"sandbox":{"credentials":{"envVars":[{"name":"TOKEN","mode":"mask","extract":"token=(.*)"}]}}}"#,
        )
        .expect("fixture JSON");
        let first = collect_native_policy_semantic_inputs(&source);
        let units = |value: &str| value.encode_utf16().collect::<Vec<_>>();
        assert_eq!(
            first.url_inputs,
            vec![
                units("https://hook.example/"),
                units("https://extra.example/"),
                units("https://registry.example/"),
                units("https://archive.example/"),
                units("https://mcp.example/"),
                units("https://bad-headers.example/"),
                units("https://list.example/"),
            ]
        );
        assert_eq!(first.display_inputs, vec![units("FutureEvent")]);
        assert_eq!(
            first.regex_patterns,
            vec![units("^host$"), units("token=(.*)")]
        );

        let registry = NativePolicyJsSemanticsRegistry::from_rows(
            vec![valid_https_row("https://mcp.example/", "mcp.example")],
            Vec::new(),
        )
        .expect("unique semantic rows");
        let second = collect_native_policy_semantic_inputs_with_js(&source, Some(&registry));
        assert!(second
            .url_inputs
            .contains(&units("https://auth.example/metadata")));
        assert!(!second
            .url_inputs
            .contains(&units("https://unvisited-auth.example/")));
        assert!(!second
            .url_inputs
            .contains(&units("https://unrelated.example")));
        assert!(!second.regex_patterns.contains(&units("unrelated")));
    }

    #[test]
    fn single_matcher_hook_shape_does_not_collect_unreached_nested_operations() {
        let source = Utf16JsonProjection::parse(
            r#"{"matcher":"*","hooks":[{"type":"http","url":"https://not-reached.example/"},{"type":"future-hook"}]}"#,
        )
        .expect("fixture JSON");
        let inputs =
            crate::settings::native_policy_hook_settings::collect_native_hook_semantic_inputs(
                &source,
            );
        assert!(inputs.url_inputs.is_empty());
        assert!(inputs.display_inputs.is_empty());
    }

    #[test]
    fn projection_ingress_keeps_json_parse_overflow_number_before_policy_validation() {
        let document = managed_file(
            r#"{"customNativeValue":1e999}"#,
            "/etc/claude-code/managed-settings.json",
        );
        assert_eq!(
            document
                .candidate_projection()
                .and_then(|projection| projection.at_pointer("/customNativeValue"))
                .and_then(JsonValueView::as_number),
            Some(f64::INFINITY),
        );
        assert_eq!(
            document
                .normalized_projection()
                .and_then(|projection| projection.at_pointer("/customNativeValue"))
                .and_then(JsonValueView::as_number),
            Some(f64::INFINITY),
        );
    }

    fn projection(selection: &NativePolicySelection) -> &Utf16JsonProjection {
        &selection
            .effective
            .as_ref()
            .expect("effective policy")
            .projection
    }

    #[test]
    fn invalid_control_values_use_native_defaults_without_dropping_siblings() {
        for value in ["null", "\"unknown\"", "false"] {
            let raw = Utf16JsonProjection::parse(&format!(
                "{{\"managedSourcesBehavior\":{value},\"allowManagedHooksOnly\":true}}"
            ))
            .expect("fixture JSON");
            let document = NativePolicyDocument::parse_managed_file(
                Some(raw.clone()),
                NativePolicyManagedFileFacts {
                    path: PathBuf::from("/managed/settings.json"),
                    load_state: NativePolicyLoadState::Loaded,
                    document_has_policy_content: None,
                },
            );
            assert!(matches!(
                document.admission(),
                NativePolicyAdmission::NativePolicyAccepted {
                    controls: NativePolicyControls {
                        managed_sources_behavior: NativeManagedSourcesBehavior::FirstWins,
                        parent_settings_behavior: NativeParentSettingsBehavior::FirstWins,
                    },
                    ..
                }
            ));
            assert_eq!(document.raw_projection(), Some(&raw));
            let normalized = document
                .normalized_projection()
                .expect("Native normalized object");
            assert!(!normalized.contains_property("managedSourcesBehavior"));
            assert_eq!(
                normalized
                    .get_property("allowManagedHooksOnly")
                    .and_then(JsonValueView::as_bool),
                Some(true)
            );
            let selection = resolve_native_policy(NativePolicyInputs {
                managed_file_documents: vec![document],
                ..NativePolicyInputs::default()
            });
            assert!(selection.complete);
            assert_eq!(
                projection(&selection)
                    .get_property("allowManagedHooksOnly")
                    .and_then(JsonValueView::as_bool),
                Some(true)
            );
        }
    }

    #[test]
    fn classifier_false_and_permission_locks_are_admitted_as_complete_managed_policy() {
        let raw = r#"{"model":"kept","autoMode":{"classifyAllShell":false},"permissions":{"disableBypassPermissionsMode":"disable","disableAutoMode":"disable"}}"#;
        let document = managed_file(raw, "/etc/claude-code/managed-settings.json");
        assert!(document.unresolved_fields().is_empty());
        assert_eq!(
            document.raw_projection().unwrap().to_json_string().unwrap(),
            raw
        );
        let normalized = document.normalized_projection().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/autoMode/classifyAllShell")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(
            normalized.string_at("/permissions/disableBypassPermissionsMode"),
            Some("disable")
        );
        assert_eq!(
            normalized.string_at("/permissions/disableAutoMode"),
            Some("disable")
        );

        let selection = resolve_native_policy(NativePolicyInputs {
            managed_file_documents: vec![document],
            ..NativePolicyInputs::default()
        });
        assert!(selection.complete);
        let effective = projection(&selection);
        assert_eq!(effective.string_at("/model"), Some("kept"));
        assert_eq!(
            effective
                .at_pointer("/autoMode/classifyAllShell")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(
            effective.string_at("/permissions/disableBypassPermissionsMode"),
            Some("disable")
        );
        assert_eq!(
            effective.string_at("/permissions/disableAutoMode"),
            Some("disable")
        );
    }

    #[test]
    fn first_wins_prefers_plist_on_equal_native_source_rank() {
        let selection = resolve_native_policy(NativePolicyInputs {
            plist_candidates: vec![plist(
                r#"{"managedSourcesBehavior":"first-wins","model":"plist-model"}"#,
                "/Library/Managed Preferences/device.plist",
                false,
            )],
            managed_file_documents: vec![managed_file(
                r#"{"model":"file-model","disableAutoMode":"disable"}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });

        assert!(selection.complete);
        assert_eq!(selection.selected_admin, Some(NativePolicySource::Plist));
        assert_eq!(selection.admin_sources, [NativePolicySource::Plist]);
        assert_eq!(
            projection(&selection).string_at("/model"),
            Some("plist-model")
        );
        assert!(projection(&selection)
            .get_property("managedSourcesBehavior")
            .is_none());
        assert!(projection(&selection)
            .get_property("disableAutoMode")
            .is_none());
    }

    #[test]
    fn merge_uses_native_order_and_applies_restrictive_values() {
        let selection = resolve_native_policy(NativePolicyInputs {
            plist_candidates: vec![plist(
                r#"{"managedSourcesBehavior":"merge","availableModels":["plist","shared"],"disableAutoMode":"disable"}"#,
                "/Library/Managed Preferences/device.plist",
                false,
            )],
            managed_file_documents: vec![managed_file(
                r#"{"availableModels":["file","shared"],"disableAutoMode":"allow"}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });

        assert!(selection.complete);
        assert_eq!(
            selection.admin_sources,
            [NativePolicySource::Plist, NativePolicySource::ManagedFile]
        );
        assert_eq!(
            projection(&selection).string_at("/disableAutoMode"),
            Some("disable")
        );
        assert_eq!(
            projection(&selection)
                .at_pointer("/availableModels")
                .and_then(JsonValueView::array_len),
            Some(3)
        );
        assert_eq!(
            projection(&selection).string_at("/availableModels/0"),
            Some("plist")
        );
    }

    #[test]
    fn empty_available_models_array_remains_a_policy_source() {
        let selection = resolve_native_policy(NativePolicyInputs {
            plist_candidates: vec![plist(
                r#"{"availableModels":[]}"#,
                "/Library/Managed Preferences/device.plist",
                false,
            )],
            ..NativePolicyInputs::default()
        });

        assert!(selection.complete);
        assert_eq!(selection.selected_admin, Some(NativePolicySource::Plist));
        assert_eq!(selection.admin_sources, [NativePolicySource::Plist]);
        assert_eq!(
            projection(&selection)
                .at_pointer("/availableModels")
                .and_then(JsonValueView::array_len),
            Some(0)
        );
    }

    #[test]
    fn substitute_only_admin_source_loses_to_authored_policy_source() {
        let selection = resolve_native_policy(NativePolicyInputs {
            plist_candidates: vec![plist(
                r#"{"allowManagedHooksOnly":"invalid"}"#,
                "/Library/Managed Preferences/device.plist",
                false,
            )],
            managed_file_documents: vec![managed_file(
                r#"{"model":"authored-file-model"}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });

        assert!(selection.complete);
        assert_eq!(
            selection.selected_admin,
            Some(NativePolicySource::ManagedFile)
        );
        assert_eq!(selection.admin_sources, [NativePolicySource::ManagedFile]);
        assert_eq!(
            projection(&selection).string_at("/model"),
            Some("authored-file-model")
        );
        assert!(projection(&selection)
            .get_property("allowManagedHooksOnly")
            .is_none());
        assert!(selection.documents[0].only_substitutes());
    }

    #[test]
    fn parent_settings_are_reduced_to_restrictive_values_before_merge() {
        let selection = resolve_native_policy(NativePolicyInputs {
            plist_candidates: vec![plist(
                r#"{"parentSettingsBehavior":"merge","model":"admin-model","permissions":{"allow":["Read(admin-only)"]},"sandbox":{"network":{"allowManagedDomainsOnly":true}}}"#,
                "/Library/Managed Preferences/device.plist",
                false,
            )],
            parent: Some(parent(
                r#"{"model":"parent-model","allowManagedHooksOnly":true,"permissions":{"deny":["Bash(rm *)","!Bash(rm safe *)"],"allow":["Bash(parent-only)"]},"sandbox":{"network":{"deniedDomains":["example.com"],"allowedDomains":["example.org"]}}}"#,
            )),
            ..NativePolicyInputs::default()
        });

        assert!(!selection.complete);
        assert!(selection.parent_included);
        assert!(selection.documents[1]
            .unresolved_fields()
            .contains(&"sandbox.network.allowedDomains".to_owned()));
        assert!(selection.documents[1]
            .unresolved_fields()
            .contains(&"sandbox.network.deniedDomains".to_owned()));
        let effective = projection(&selection);
        assert_eq!(effective.string_at("/model"), Some("admin-model"));
        assert_eq!(
            effective
                .get_property("allowManagedHooksOnly")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            effective.string_at("/permissions/deny/0"),
            Some("Bash(rm *)")
        );
        assert_eq!(
            effective.string_at("/permissions/allow/0"),
            Some("Read(admin-only)")
        );
        assert!(effective.at_pointer("/permissions/allow/1").is_none());
        assert_eq!(
            effective.string_at("/sandbox/network/deniedDomains/0"),
            Some("example.com")
        );
        assert!(effective
            .at_pointer("/sandbox/network/allowedDomains")
            .is_none());
    }

    #[test]
    fn parent_defaults_to_first_wins_but_can_enter_as_restrictive_slice() {
        let selection = resolve_native_policy(NativePolicyInputs {
            plist_candidates: vec![plist(
                r#"{"model":"admin-model"}"#,
                "/Library/Managed Preferences/device.plist",
                false,
            )],
            parent: Some(parent(
                r#"{"model":"parent-model","allowManagedHooksOnly":true}"#,
            )),
            ..NativePolicyInputs::default()
        });

        assert!(!selection.parent_included);
        assert!(projection(&selection)
            .get_property("allowManagedHooksOnly")
            .is_none());
    }

    #[test]
    fn empty_host_fallback_is_not_a_policy_document() {
        let empty = resolve_native_policy(NativePolicyInputs {
            host: Some(host("{}")),
            ..NativePolicyInputs::default()
        });
        assert!(empty.effective.is_none());
        assert!(!empty.host_fallback_used);

        let nonempty = resolve_native_policy(NativePolicyInputs {
            host: Some(host(r#"{"model":"host-model"}"#)),
            ..NativePolicyInputs::default()
        });
        assert!(nonempty.host_fallback_used);
        assert_eq!(
            projection(&nonempty).string_at("/model"),
            Some("host-model")
        );
    }

    #[test]
    fn user_writable_plist_source_fact_does_not_rewrite_policy_document() {
        let document = plist(
            r#"{"managedMcpServers":{"server":{}},"isolation":{"required":true},"prependPlugins":["x"],"appendPlugins":["y"],"enabledPlugins":{"a":true,"b":false,"c@builtin":false}}"#,
            "/Library/Managed Preferences/user.plist",
            true,
        );
        let candidate = document
            .candidate_projection()
            .expect("candidate projection");
        assert!(candidate.get_property("managedMcpServers").is_some());
        assert!(candidate.get_property("isolation").is_some());
        assert!(candidate.get_property("prependPlugins").is_some());
        assert!(candidate.get_property("appendPlugins").is_some());
        assert_eq!(
            candidate
                .get_property("enabledPlugins")
                .and_then(|value| value.get_property("a"))
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            candidate
                .get_property("enabledPlugins")
                .and_then(|value| value.get_property("b"))
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(
            candidate
                .get_property("enabledPlugins")
                .and_then(|value| value.get_property("c@builtin"))
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(document
            .raw_projection()
            .expect("raw source")
            .get_property("managedMcpServers")
            .is_some());
    }

    #[test]
    fn invalid_enabled_plugins_field_is_ignored_and_keeps_siblings() {
        let raw = Utf16JsonProjection::parse(r#"{"enabledPlugins":"invalid","model":"kept"}"#)
            .expect("fixture JSON");
        let document = NativePolicyDocument::parse_managed_file(
            Some(raw.clone()),
            NativePolicyManagedFileFacts {
                path: PathBuf::from("/etc/claude-code/managed-settings.json"),
                load_state: NativePolicyLoadState::Loaded,
                document_has_policy_content: Some(true),
            },
        );
        let selection = resolve_native_policy(NativePolicyInputs {
            managed_file_documents: vec![document],
            ..NativePolicyInputs::default()
        });
        assert!(selection.complete);
        assert_eq!(selection.documents.len(), 1);
        assert_eq!(selection.documents[0].raw_projection(), Some(&raw));
        assert_eq!(
            selection.selected_admin,
            Some(NativePolicySource::ManagedFile)
        );
        assert!(selection.documents[0].unresolved_fields().is_empty());
        let effective = &selection
            .effective
            .as_ref()
            .expect("Native uses the parsed policy projection")
            .projection;
        assert!(effective.get_property("enabledPlugins").is_none());
        assert_eq!(effective.string_at("/model"), Some("kept"));
    }

    #[test]
    fn unresolved_native_field_stays_visible_and_marks_result_incomplete() {
        let raw = Utf16JsonProjection::parse(r#"{"isolation":{"required":true},"model":"kept"}"#)
            .expect("fixture JSON");
        let unresolved = NativePolicyDocument::parse_managed_file(
            Some(raw.clone()),
            NativePolicyManagedFileFacts {
                path: PathBuf::from("/etc/claude-code/managed-settings.json"),
                load_state: NativePolicyLoadState::Loaded,
                document_has_policy_content: Some(true),
            },
        );
        let selection = resolve_native_policy(NativePolicyInputs {
            managed_file_documents: vec![unresolved],
            ..NativePolicyInputs::default()
        });
        assert!(!selection.complete);
        assert_eq!(selection.documents.len(), 1);
        assert_eq!(selection.documents[0].raw_projection(), Some(&raw));
        assert_eq!(
            selection.selected_admin,
            Some(NativePolicySource::ManagedFile)
        );
        assert_eq!(selection.documents[0].unresolved_fields().len(), 1);
        assert_eq!(
            selection.documents[0].unresolved_fields()[0].as_str(),
            "isolation.required"
        );
        let effective = &selection
            .effective
            .as_ref()
            .expect("Native uses the parsed policy projection")
            .projection;
        assert_eq!(
            effective
                .get_property("isolation")
                .and_then(|value| value.get_property("required"))
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(effective.string_at("/model"), Some("kept"));
    }

    #[test]
    fn helper_plan_keeps_wsl_chain_and_static_fallback_source_order() {
        let selection = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Wsl,
            managed_file_documents: vec![managed_file(
                r#"{
                    "model":"managed-model",
                    "policyHelper":{"path":"/usr/local/bin/singular"},
                    "policyHelpers":{
                        "wsl":{"path":"/usr/local/bin/wsl-helper","outputBehavior":"merge","defaultSettings":{"model":"wsl-default"}},
                        "linux":{"path":"/usr/local/bin/linux-helper","defaultSettings":{"model":"linux-default"}},
                        "macos":{"defaultSettings":{"model":"macos-default"}},
                        "windows":{"defaultSettings":{"model":"windows-default"}},
                        "default":{"model":"map-default"}
                    }
                }"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });

        let plan = selection.helper_plan.as_ref().expect("admin helper plan");
        assert_eq!(plan.source, NativePolicySource::ManagedFile);
        assert_eq!(
            plan.source_paths,
            [PathBuf::from("/etc/claude-code/managed-settings.json")]
        );
        assert_eq!(plan.platform_chain, ["wsl", "linux"]);
        let selected = plan
            .selected_entry
            .as_ref()
            .expect("first executable on WSL chain");
        assert_eq!(selected.entry_key, "wsl");
        assert_eq!(
            selected.config.string_at("/path"),
            Some("/usr/local/bin/wsl-helper")
        );
        assert_eq!(selected.config.string_at("/outputBehavior"), Some("merge"));
        assert!(selected.config.at_pointer("/defaultSettings").is_none());

        let fallback_fields = plan
            .static_fallback_candidates
            .iter()
            .map(|candidate| candidate.source_field.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            fallback_fields,
            [
                "policyHelpers.wsl.defaultSettings",
                "policyHelpers.linux.defaultSettings",
                "policyHelpers.macos.defaultSettings",
                "policyHelpers.windows.defaultSettings",
                "policyHelpers.default",
            ]
        );
        assert!(plan.static_fallback_candidates[0].on_chain);
        assert!(plan.static_fallback_candidates[0].merges_output);
        assert!(plan.static_fallback_candidates[1].on_chain);
        assert!(!plan.static_fallback_candidates[2].on_chain);
        assert!(selection.complete);
        assert!(plan.static_fallback_candidates.iter().all(|candidate| {
            candidate.validation == NativePolicyStaticFallbackValidation::Validated
                && candidate.normalized_settings.is_some()
        }));
    }

    #[test]
    fn off_chain_static_due_failure_is_fatal_after_all_er_candidates_are_checked() {
        let selection = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::MacOS,
            managed_file_documents: vec![managed_file(
                r#"{"policyHelpers":{"macos":{"defaultSettings":{"model":"ok"}},"linux":{"defaultSettings":{"permissions":{"defaultMode":"bad"}}}}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let plan = selection.helper_plan.as_ref().expect("admin helper plan");
        assert_eq!(plan.static_fallback_candidates.len(), 2);
        assert!(matches!(
            plan.static_fallback_candidates[0].validation,
            NativePolicyStaticFallbackValidation::Validated
        ));
        assert!(matches!(
            plan.static_fallback_candidates[1].validation,
            NativePolicyStaticFallbackValidation::Rejected
        ));
        let fatal = selection
            .startup_fatal_helper_paths
            .iter()
            .find(|fatal| fatal.path == "policyHelpers.linux.defaultSettings")
            .expect("off-chain invalid candidate is part of Native preflight");
        assert!(fatal
            .diagnostic
            .as_ref()
            .and_then(|value| value.string_units(""))
            .is_some());
    }

    #[test]
    fn static_fallback_apply_uses_only_the_selected_normalized_candidate() {
        let selection = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"policyHelpers":{"linux":{"defaultSettings":{"model":"fallback"}}}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let plan = selection.helper_plan.as_ref().expect("helper plan");
        let field = "policyHelpers.linux.defaultSettings";
        assert_eq!(plan.selected_static_fallback_field.as_deref(), Some(field));
        let candidate = plan
            .static_fallback_candidates
            .iter()
            .find(|candidate| candidate.source_field == field)
            .expect("selected candidate");
        assert_eq!(
            candidate.validation,
            NativePolicyStaticFallbackValidation::Validated
        );
        assert_eq!(
            candidate
                .normalized_settings
                .as_ref()
                .and_then(|settings| settings.string_at("/model")),
            Some("fallback")
        );

        let applied = apply_native_policy_helper_settings(
            &selection,
            NativePolicySource::ManagedFile,
            &NativePolicyHelperSettingsInput::StaticFallback {
                source_field: field.to_owned(),
            },
        );
        assert_eq!(applied.status, NativePolicyHelperApplyStatus::Applied);
        assert_eq!(
            projection(&applied.selection).string_at("/model"),
            Some("fallback")
        );

        let forged = apply_native_policy_helper_settings(
            &selection,
            NativePolicySource::ManagedFile,
            &NativePolicyHelperSettingsInput::StaticFallback {
                source_field: "policyHelpers.windows.defaultSettings".to_owned(),
            },
        );
        assert_eq!(forged.status, NativePolicyHelperApplyStatus::SourceMismatch);
    }

    #[test]
    fn helper_plan_orders_static_fallback_before_singular_helper() {
        let with_static = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"model":"base","policyHelper":{"path":"/usr/local/bin/singular"},"policyHelpers":{"linux":{"defaultSettings":{"model":"fallback"}}}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let plan = with_static.helper_plan.as_ref().expect("helper plan");
        assert_eq!(
            plan.selected_kind,
            NativePolicyHelperSelectedKind::StaticFallback
        );
        assert_eq!(
            plan.selected_static_fallback_field.as_deref(),
            Some("policyHelpers.linux.defaultSettings")
        );
        assert!(plan.singular_entry.is_some());
        assert_eq!(
            plan.static_fallback_candidates[0].validation,
            NativePolicyStaticFallbackValidation::Validated
        );
        assert_eq!(
            plan.static_fallback_candidates[0]
                .normalized_settings
                .as_ref()
                .and_then(|value| value.string_at("/model")),
            Some("fallback")
        );

        let singular_only = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"model":"base","policyHelper":{"path":"/usr/local/bin/singular","timeoutMs":2000}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let plan = singular_only.helper_plan.as_ref().expect("singular plan");
        assert_eq!(
            plan.selected_kind,
            NativePolicyHelperSelectedKind::SingularHelper
        );
        let singular = plan
            .singular_entry
            .as_ref()
            .expect("singular helper config");
        assert!(!singular.from_per_os);
        assert_eq!(singular.entry_key, "policyHelper");
        assert_eq!(singular.timeout_ms, 2000);
        assert_eq!(singular.on_failure, NativePolicyHelperOnFailure::Refuse);
    }

    #[test]
    fn helper_plan_exposes_native_effective_config_defaults_and_startup_fatal() {
        let selection = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"model":"base","policyHelpers":{"linux":{"path":"/usr/local/bin/policy-helper","onFailure":"continue","outputBehavior":"merge","timeoutMs":1500,"retries":4,"refreshIntervalMs":60000}}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let plan = selection.helper_plan.as_ref().expect("helper plan");
        assert!(selection.complete);
        assert!(plan.startup_fatal_paths.is_empty());
        assert_eq!(
            plan.selected_kind,
            NativePolicyHelperSelectedKind::PerOsHelper
        );
        assert_eq!(plan.validation, NativePolicyHelperPlanValidation::Validated);
        let helper = plan.selected_entry.as_ref().expect("Linux helper");
        assert!(helper.from_per_os);
        assert_eq!(helper.entry_key, "linux");
        assert_eq!(helper.on_failure, NativePolicyHelperOnFailure::Continue);
        assert_eq!(helper.timeout_ms, 1500);
        assert_eq!(helper.retries, 4);
        assert_eq!(helper.refresh_interval_ms, 60000);
        assert_eq!(
            helper.output_behavior,
            NativePolicyHelperOutputBehavior::Merge
        );

        let surrogate_path = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"model":"base","policyHelpers":{"linux":{"path":"/usr/local/bin/helper-\ud800"}}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let selected = surrogate_path
            .helper_plan
            .as_ref()
            .and_then(|plan| plan.selected_entry.as_ref())
            .expect("Native-valid POSIX helper path remains selectable");
        assert!(surrogate_path.complete);
        assert_eq!(
            selected.config.string_units("/path"),
            Some(vec![
                b'/' as u16,
                b'u' as u16,
                b's' as u16,
                b'r' as u16,
                b'/' as u16,
                b'l' as u16,
                b'o' as u16,
                b'c' as u16,
                b'a' as u16,
                b'l' as u16,
                b'/' as u16,
                b'b' as u16,
                b'i' as u16,
                b'n' as u16,
                b'/' as u16,
                b'h' as u16,
                b'e' as u16,
                b'l' as u16,
                b'p' as u16,
                b'e' as u16,
                b'r' as u16,
                b'-' as u16,
                0xd800,
            ])
        );

        let startup_fatal = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"model":"base","policyHelpers":[]}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let plan = startup_fatal
            .helper_plan
            .as_ref()
            .expect("fatal plan retained");
        assert!(
            startup_fatal.complete,
            "parsing a fatal source result is complete"
        );
        assert_eq!(plan.startup_fatal_paths, vec!["policyHelpers".to_owned()]);
        assert_eq!(plan.selected_kind, NativePolicyHelperSelectedKind::NoEntry);
        let apply = apply_native_policy_helper_settings(
            &startup_fatal,
            NativePolicySource::ManagedFile,
            &NativePolicyHelperSettingsInput::Dynamic {
                settings: None,
                behavior: NativePolicyHelperOutputBehavior::Replace,
                semantic_evidence: empty_native_policy_semantics(),
            },
        );
        assert_eq!(
            apply.status,
            NativePolicyHelperApplyStatus::StartupFatal {
                diagnostics: startup_fatal.startup_fatal_helper_paths.clone()
            }
        );
        assert_eq!(
            startup_fatal.startup_fatal_helper_paths[0]
                .diagnostic
                .as_ref()
                .and_then(|value| value.at_pointer("").and_then(JsonValueView::as_str)),
            Some(
                "\"policyHelpers\" could not be parsed: expected an object mapping OS keys (macos, linux, windows, wsl) to helper entries, plus an optional \"default\" settings payload (Invalid input: expected object, received array). When delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed."
            )
        );
    }

    #[test]
    fn startup_fatal_helper_errors_include_nonwinner_admin_sources_only() {
        let expected_message = "\"policyHelpers\" could not be parsed: expected an object mapping OS keys (macos, linux, windows, wsl) to helper entries, plus an optional \"default\" settings payload (Invalid input: expected object, received array). When delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed.";
        let with_nonwinner_fatal = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            plist_candidates: vec![plist(
                r#"{"model":"plist-model"}"#,
                "/Library/Preferences/com.example.code.plist",
                false,
            )],
            managed_file_documents: vec![managed_file(
                r#"{"model":"file-model","policyHelpers":[]}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        assert_eq!(
            with_nonwinner_fatal.selected_admin,
            Some(NativePolicySource::Plist)
        );
        assert!(with_nonwinner_fatal.helper_plan.is_none());
        assert_eq!(
            with_nonwinner_fatal.startup_fatal_helper_paths,
            vec![NativePolicyStartupFatal {
                source: NativePolicySource::ManagedFile,
                source_paths: vec![PathBuf::from("/etc/claude-code/managed-settings.json")],
                path: "policyHelpers".into(),
                diagnostic: Some(
                    Utf16JsonProjection::root_string(
                        expected_message.to_owned(),
                        expected_message.encode_utf16().collect(),
                    )
                    .unwrap(),
                ),
            }]
        );

        let user_writable = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            plist_candidates: vec![plist(
                r#"{"model":"user-model","policyHelpers":[]}"#,
                "/Users/test/Library/Preferences/com.example.code.plist",
                true,
            )],
            ..NativePolicyInputs::default()
        });
        assert!(user_writable.startup_fatal_helper_paths.is_empty());
        assert!(user_writable.helper_plan.is_none());
    }

    #[test]
    fn helper_output_apply_recomposes_from_immutable_base_and_keeps_utf16_envelope() {
        let selection = resolve_native_policy(NativePolicyInputs {
            platform: NativePolicyPlatform::Linux,
            managed_file_documents: vec![managed_file(
                r#"{"model":"base","nested":{"authored":true},"policyHelpers":{"linux":{"path":"/usr/local/bin/policy-helper","outputBehavior":"merge"}}}"#,
                "/etc/claude-code/managed-settings.json",
            )],
            ..NativePolicyInputs::default()
        });
        let envelope = Utf16JsonProjection::parse(
            r#"{"managedSettings":{"model":"first","nested":{"first":true}},"claudeMd":"rule \ud800","appendSystemPrompt":"append"}"#,
        )
        .expect("helper envelope projection");
        let parsed = parse_native_policy_helper_output(&envelope).expect("valid envelope");
        assert_eq!(parsed.raw, envelope);
        assert_eq!(
            parsed.claude_md.as_ref().unwrap().string_units(""),
            Some(vec![
                b'r' as u16,
                b'u' as u16,
                b'l' as u16,
                b'e' as u16,
                b' ' as u16,
                0xd800,
            ])
        );

        let first = apply_native_policy_helper_settings(
            &selection,
            NativePolicySource::ManagedFile,
            &NativePolicyHelperSettingsInput::Dynamic {
                settings: parsed.managed_settings.clone(),
                behavior: NativePolicyHelperOutputBehavior::Merge,
                semantic_evidence: empty_native_policy_semantics(),
            },
        );
        assert_eq!(first.status, NativePolicyHelperApplyStatus::Applied);
        let first_projection = projection(&first.selection);
        assert_eq!(first_projection.string_at("/model"), Some("first"));
        assert_eq!(
            first_projection
                .at_pointer("/nested/authored")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            first_projection
                .at_pointer("/nested/first")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );

        let second_settings =
            Utf16JsonProjection::parse(r#"{"model":"second","nested":{"second":true}}"#)
                .expect("second helper output");
        let second = apply_native_policy_helper_settings(
            &first.selection,
            NativePolicySource::ManagedFile,
            &NativePolicyHelperSettingsInput::Dynamic {
                settings: Some(second_settings.clone()),
                behavior: NativePolicyHelperOutputBehavior::Merge,
                semantic_evidence: empty_native_policy_semantics(),
            },
        );
        assert_eq!(second.status, NativePolicyHelperApplyStatus::Applied);
        let second_projection = projection(&second.selection);
        assert_eq!(second_projection.string_at("/model"), Some("second"));
        assert_eq!(
            second_projection
                .at_pointer("/nested/authored")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert!(second_projection.at_pointer("/nested/first").is_none());
        assert_eq!(
            second_projection
                .at_pointer("/nested/second")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
    }

    #[test]
    fn helper_output_envelope_errors_match_pinned_zod_messages() {
        // Oracle: the extracted 2.1.295 Hi() schema was executed under Bun
        // 1.3.11 from metadata-source/native-bundle/src_186578912.mjs after
        // initializing its source-required __zod_globalConfig object.
        let array = Utf16JsonProjection::parse("[]").expect("valid JSON array");
        let NativePolicyHelperOutputError::InvalidEnvelope { diagnostic } =
            parse_native_policy_helper_output(&array).expect_err("arrays reach Hi()")
        else {
            panic!("arrays use Native's envelope validator");
        };
        assert_eq!(
            diagnostic.string_at(""),
            Some(
                "[\n  {\n    \"expected\": \"object\",\n    \"code\": \"invalid_type\",\n    \"path\": [],\n    \"message\": \"Invalid input: expected object, received array\"\n  }\n]"
            )
        );

        let fields = Utf16JsonProjection::parse(r#"{"claudeMd":1,"appendSystemPrompt":false}"#)
            .expect("valid JSON object");
        let NativePolicyHelperOutputError::InvalidEnvelope { diagnostic } =
            parse_native_policy_helper_output(&fields).expect_err("string fields are typed")
        else {
            panic!("invalid envelope fields are reported together");
        };
        assert_eq!(
            diagnostic.string_at(""),
            Some(
                "[\n  {\n    \"expected\": \"string\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"claudeMd\"\n    ],\n    \"message\": \"Invalid input: expected string, received number\"\n  },\n  {\n    \"expected\": \"string\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"appendSystemPrompt\"\n    ],\n    \"message\": \"Invalid input: expected string, received boolean\"\n  }\n]"
            )
        );

        let any_managed_settings =
            Utf16JsonProjection::parse(r#"{"managedSettings":null,"claudeMd":"ok"}"#)
                .expect("valid JSON object");
        let parsed = parse_native_policy_helper_output(&any_managed_settings)
            .expect("Hi() leaves managedSettings for qn()");
        assert_eq!(
            parsed
                .managed_settings
                .as_ref()
                .unwrap()
                .to_json_string()
                .unwrap(),
            "null"
        );
    }

    #[test]
    fn helper_error_text_removes_native_control_categories_but_keeps_tab_and_lf() {
        assert_eq!(
            sanitize_native_policy_helper_error_text("a\t\nb\r\u{200b}c\u{2028}d\u{2029}e"),
            "a\t\nbcde"
        );
    }
}
