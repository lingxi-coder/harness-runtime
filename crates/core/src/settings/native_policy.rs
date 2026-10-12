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

use super::native_policy_parser::{
    parse_policy_document, NativePolicyParseOptions, NativePolicyParseStatus,
};
use crate::settings::SettingsJson;
use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};
use std::path::PathBuf;

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
        let user_writable = facts.user_writable;
        Self::parse(
            NativePolicySource::Plist,
            NativePolicyOrigin::Plist(facts),
            raw_projection,
            user_writable,
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
            false,
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
            false,
        )
    }

    fn parse(
        source: NativePolicySource,
        origin: NativePolicyOrigin,
        raw_projection: Option<Utf16JsonProjection>,
        user_writable: bool,
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
                admission: NativePolicyAdmission::Unavailable { load_state },
            };
        };

        let parser_result = match raw.to_json_string() {
            Ok(raw_text) => {
                parse_policy_document(&raw_text, NativePolicyParseOptions { user_writable })
            }
            Err(error) => {
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
                    admission: NativePolicyAdmission::NativePolicyRejected {
                        reason: format!("source projection cannot be serialized for Native policy parsing: {error}"),
                    },
                };
            }
        };
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
        let admission = if !parser_result.document_admitted {
            NativePolicyAdmission::NativePolicyRejected {
                reason: match parser_result.status {
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
                projection: plist
                    .normalized_projection
                    .as_ref()
                    .expect("admitted plist has projection")
                    .clone(),
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
                    projection: file_projection.clone(),
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
        Some(compose_admin_tiers(&contributing))
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
            .and_then(|_| document.normalized_projection.as_ref())
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
        (None, _) => {
            let host = inputs.host.as_ref().and_then(|document| {
                document
                    .controls()
                    .and_then(|_| document.normalized_projection.as_ref())
            });
            match host.filter(|projection| !is_empty_object(projection)) {
                Some(projection) => (Some(projection.clone()), true),
                None => (None, false),
            }
        }
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
    NativePolicySelection {
        effective,
        selected_admin,
        held_empty_source,
        admin_sources,
        parent_included,
        host_fallback_used,
        complete: unresolved.is_empty(),
        documents,
        unresolved,
    }
}

#[derive(Debug, Clone)]
struct AdminCandidate {
    source: NativePolicySource,
    projection: Utf16JsonProjection,
    rank: i8,
    controls: NativePolicyControls,
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

fn compose_admin_tiers(tiers: &[AdminCandidate]) -> Utf16JsonProjection {
    let mut selected = empty_object();
    for tier in tiers.iter().rev() {
        selected = native_merge(&selected, &tier.projection);
    }
    apply_admin_restrictions(&mut selected, tiers);
    strip_managed_sources_behavior(&selected)
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

fn enum_field<'a>(
    projection: &'a Utf16JsonProjection,
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
    fn user_writable_plist_removes_admin_only_fields_and_keeps_only_disables() {
        let document = plist(
            r#"{"managedMcpServers":{"server":{}},"isolation":{"required":true},"prependPlugins":["x"],"appendPlugins":["y"],"enabledPlugins":{"a":true,"b":false,"c@builtin":false}}"#,
            "/Library/Managed Preferences/user.plist",
            true,
        );
        let candidate = document
            .candidate_projection()
            .expect("candidate projection");
        assert!(candidate.get_property("managedMcpServers").is_none());
        assert!(candidate.get_property("isolation").is_none());
        assert!(candidate.get_property("prependPlugins").is_none());
        assert_eq!(
            candidate
                .get_property("enabledPlugins")
                .and_then(|value| value.get_property("b"))
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(candidate
            .get_property("enabledPlugins")
            .and_then(|value| value.get_property("a"))
            .is_none());
        assert!(candidate
            .get_property("enabledPlugins")
            .and_then(|value| value.get_property("c@builtin"))
            .is_none());
        assert!(document
            .raw_projection()
            .expect("raw source")
            .get_property("managedMcpServers")
            .is_some());
    }

    #[test]
    fn unresolved_native_field_stays_visible_and_marks_result_incomplete() {
        let raw = Utf16JsonProjection::parse(r#"{"enabledPlugins":"invalid","model":"bad"}"#)
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
            "enabledPlugins"
        );
        let effective = &selection
            .effective
            .as_ref()
            .expect("Native uses the parsed policy projection")
            .projection;
        assert_eq!(effective.string_at("/enabledPlugins"), Some("invalid"));
        assert_eq!(effective.string_at("/model"), Some("bad"));
    }
}
