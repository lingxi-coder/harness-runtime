use platform_api::AuthHandle;
#[cfg(windows)]
use platform_windows::process::supervisor as shell_supervisor;
#[cfg(windows)]
use platform_windows::WindowsMcpTransport;
use secret::CredentialManager;
use std::sync::Arc;
use std::sync::Mutex;

use super::{
    fusion_attempts, load_effective_settings_for_config, managed_model_policy_source,
    managed_settings_raw_tiers_sync, DesktopConfig,
};

/// `billing_mode` is the OWNING profile's `PricingConfig.billing_mode` (the
/// same bit `provider-config::cost_translate` keys `mark_unpriced` on).
///
/// Finding [2]: the checked-in `llm_runtime::fusion_hints` table only carries
/// a handful of rows per subscription-billed profile (e.g. 8 of
/// `github-copilot`'s 36 slice models). Every row that table has no entry
/// for defaults to `FusionModelHints::default()` (`cost_class: Medium`), so
/// without this override such a model is priced through
/// `DesktopFusionPriceBook::rates_for`, which the composition root has
/// deliberately marked `explicitly_unpriced` for every Subscription-mode
/// profile — `budget::model_peak` then hard-rejects any capped session that
/// selects it (`"token-billed model \`.../...\` has no price"`), even though
/// the model is subscription-billed and would reserve/settle at $0. Forcing
/// `cost_class: Subscription` here whenever the owning profile is
/// subscription-billed is inert for the rows the hint table already lists
/// for such a profile — every one of them already hand-codes
/// `FusionCostClass::Subscription` — so this only changes the
/// previously-unhinted rows that were wrongly defaulting to `Medium`.
pub(super) fn desktop_fusion_catalog_row(
    profile: &str,
    model: &llm_runtime::ModelProfile,
    billing_mode: platform_api::ModelBillingMode,
    protocol: &llm_runtime::ProtocolFamily,
) -> fusion::CatalogModel {
    let mut hints = llm_runtime::hints_for(profile, &model.request_model).unwrap_or_default();
    if billing_mode == platform_api::ModelBillingMode::Subscription {
        hints.cost_class = platform_api::FusionCostClass::Subscription;
    }
    fusion::CatalogModel {
        profile: profile.to_string(),
        model: model.request_model.clone(),
        hints,
        // Round-5 review finding [3]: `capabilities.structured_output` is a
        // property of the MODEL (copied verbatim from the vendored
        // models.dev slice); `CatalogModel::structured_output` is the
        // stronger claim `resolve_analyst`'s `with_schema` gate needs — that
        // THIS profile can actually put a `response_format` on the wire.
        // AND-ing the owning profile's codec in is what makes the two agree.
        structured_output: model.capabilities.structured_output
            && protocol.encodes_response_format(),
        limits: fusion::ModelLimits::from_metadata(&model.metadata),
    }
}

/// F011 item 1: drop every catalog row a Fusion panel cannot actually reach —
/// an uncredentialed provider profile, or a model a managed
/// `enforceAvailableModels` policy has barred — BEFORE it can ever reach
/// `model_resolver::resolve`. Without this, an automatic preset can select a
/// provider with no credential (or a managed-barred model), and a panel
/// burns turns before failing at request time (`LlmError::Authentication`)
/// instead of failing the §4 preflight with zero provider calls.
///
/// `anthropic_probe_definitive` gives the availability half of this filter
/// the same probe-blindness guard `connected_provider_fallback` has (see its
/// doc comment). On a gateway / env-routed Bedrock/Vertex/Foundry install,
/// `provider_availability["anthropic"] == false` reflects only that the local
/// key/OAuth probe is BLIND, not that anthropic is disconnected — Claude
/// models are served fine there, and the main turn loop routes them. Dropping
/// anthropic rows on that signal emptied the Fusion catalog on every such
/// install (`TooFewModels{eligible:0}` on every `/fusion` call) even though
/// the same models work for the ordinary turn loop. When the probe is not
/// definitive, anthropic rows are kept regardless of the availability map;
/// every other profile's absence/`false` still means genuinely unavailable —
/// UNLESS the whole probe timed out (`availability_probe_completed ==
/// false`), in which case an empty map cannot be told apart from "every
/// non-anthropic provider is uncredentialed" and the availability half of
/// this filter is skipped entirely rather than fail-closing the whole
/// Fusion catalog on a transient stall (finding [7]).
pub(super) fn filter_fusion_catalog(
    catalog: Vec<fusion::CatalogModel>,
    provider_availability: &std::collections::BTreeMap<String, bool>,
    anthropic_probe_definitive: bool,
    availability_probe_completed: bool,
    session_model_restriction: Option<&(
        llm_runtime::model::allowlist::ModelEnforcement,
        Vec<String>,
    )>,
) -> Vec<fusion::CatalogModel> {
    catalog
        .into_iter()
        .filter(|row| {
            // Finding [7]: an empty `provider_availability` map means either
            // "the probe ran and found nothing" (genuinely uncredentialed —
            // fail closed) or "the whole probe timed out" (unknown — fail
            // open, same as the sibling `connected_provider_fallback` rule
            // treats an absent map entry). Without this branch the timeout
            // case is indistinguishable from the first and drops every
            // non-anthropic profile's rows for the runtime's lifetime.
            if !availability_probe_completed {
                return true;
            }
            if row.profile == "anthropic" && !anthropic_probe_definitive {
                return true;
            }
            provider_availability
                .get(&row.profile)
                .copied()
                .unwrap_or(false)
        })
        .filter(|row| match session_model_restriction {
            None => true,
            Some((enforcement, _)) => {
                llm_runtime::model::allowlist::model_allowed_under(enforcement, &row.model)
                    != Some(false)
            }
        })
        .collect()
}

/// Round-4 review finding [8]: `filter_fusion_catalog`'s availability input
/// (`provider_availability`) used to be baked into a plain `Vec<CatalogModel>`
/// at `desktop_fusion_executor` construction — a boot-time snapshot frozen
/// for the process lifetime. A provider credentialed mid-session via
/// `/connect` (or a completed `/login`) therefore stayed invisible to Fusion
/// until a restart, even though the ordinary turn loop routes the same
/// credential on the very next request (`MultiCredentialProvider` reads
/// `CredentialManager` per call, not a boot snapshot).
///
/// This is the `ModelSource` half of the fix, mirroring `DesktopFusionConfigSource`
/// (F007, which already re-resolves `fusion.*` SETTINGS on every call instead
/// of freezing them at construction — see its doc comment): `list()` re-runs
/// `filter_fusion_catalog` against whatever `availability` holds RIGHT NOW,
/// not a value captured at construction. `availability` starts as the
/// boot-time probe result and is updated in place by
/// [`FusionCatalogRefresher::refresh`], which a credential-write path calls
/// after persisting a new credential.
///
/// `unfiltered` and `anthropic_probe_definitive` stay fixed for the process.
/// Provider availability is shared mutable state, while the managed model
/// restriction is re-resolved for each view so a policy edit can tighten or
/// relax the next preparation without mutating an accepted snapshot.
///
/// Round-7 finding [2]: `availability_probe_completed` used to be a plain
/// `bool` frozen at construction, and nothing in the process ever set it to
/// `true`. A single 5s boot-probe stall therefore disabled the availability
/// half of `filter_fusion_catalog` for the runtime's LIFETIME — including
/// after [`FusionCatalogRefresher::refresh_inner`] had re-run the same probe
/// over the full boot credential-source list and published a complete,
/// authoritative map into the very lock `list()` reads. Every `/fusion` in
/// that process kept reserving budget for, spawning and failing panels on
/// providers with no credential. It is now a shared cell the refresher can
/// re-arm — see `refresh_inner` for why re-arming is gated on a probe whose
/// result a degraded credential backend could not have produced.
pub(super) struct FusionCatalogModelSource {
    pub(super) unfiltered: Vec<fusion::CatalogModel>,
    pub(super) availability: Arc<std::sync::RwLock<std::collections::BTreeMap<String, bool>>>,
    pub(super) anthropic_probe_definitive: bool,
    pub(super) availability_probe_completed: Arc<std::sync::atomic::AtomicBool>,
    pub(super) mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(super) session_model_restriction:
        Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)>,
    /// Production reloads managed model policy on every catalog view. Tests
    /// that inject a fixed restriction keep this false for deterministic,
    /// filesystem-independent fixtures.
    pub(super) reload_managed_model_restriction: bool,
}

impl fusion::ModelSource for FusionCatalogModelSource {
    fn list(&self) -> Vec<fusion::CatalogModel> {
        let availability = self
            .availability
            .read()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        let live_restriction = self
            .reload_managed_model_restriction
            .then(live_managed_model_restriction_sync)
            .flatten();
        let restriction = if self.reload_managed_model_restriction {
            live_restriction.as_ref()
        } else {
            self.session_model_restriction.as_ref()
        };
        filter_fusion_catalog(
            self.unfiltered.clone(),
            &availability,
            self.anthropic_probe_definitive,
            self.availability_probe_completed
                .load(std::sync::atomic::Ordering::Acquire),
            restriction,
        )
    }

    fn revision(&self) -> u64 {
        self.mutation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(super) fn live_managed_model_restriction_sync(
) -> Option<(llm_runtime::model::allowlist::ModelEnforcement, Vec<String>)> {
    use llm_runtime::model::allowlist::{self, ModelEnforcement};

    let source = managed_model_policy_source(&managed_settings_raw_tiers_sync());
    let enforcement = allowlist::resolve_enforcement(&source, &mut |_| {});
    match enforcement {
        ModelEnforcement::Inactive => None,
        // `Refused` is intentionally retained: a malformed managed policy
        // must remove every Fusion route rather than fail open.
        active_or_refused => Some((active_or_refused, Vec::new())),
    }
}

/// One shared, mutable route flag for [`FusionCatalogRefresher`] — see the
/// doc comment on its `anthropic_has_api_key` field for why the three
/// special-cased credential slots stopped being construction-time booleans.
pub(super) fn fusion_route_flag(initial: bool) -> Arc<std::sync::atomic::AtomicBool> {
    Arc::new(std::sync::atomic::AtomicBool::new(initial))
}

/// Handle a credential-write path (`/connect` completion, a finished
/// `/login`) calls after persisting a new credential, so Fusion's catalog
/// filter (`FusionCatalogModelSource`) sees it within the same process —
/// see that type's doc comment for the finding this closes. Cheap to clone:
/// the mutable state lives behind the shared `availability` lock.
#[derive(Clone)]
pub struct FusionCatalogRefresher {
    pub(super) availability: Arc<std::sync::RwLock<std::collections::BTreeMap<String, bool>>>,
    /// The SAME cell [`FusionCatalogModelSource`] reads (round-7 finding
    /// [2]). A re-probe whose result cannot have been corrupted by a
    /// degraded credential backend re-arms the availability filter a
    /// timed-out boot probe had disabled; see `refresh_inner`.
    pub(super) availability_probe_completed: Arc<std::sync::atomic::AtomicBool>,
    /// Monotonic credential-mutation epoch shared by every clone of this
    /// refresher.  A background availability probe may outlive the write that
    /// started it; checking this epoch while holding the availability write
    /// lock prevents that stale probe from publishing over a newer delete (or
    /// rotation).
    pub(super) mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
    pub(super) credentials: Arc<CredentialManager>,
    pub(super) credential_sources: Vec<provider_config::CredentialSource>,
    /// Live availability for the three routes
    /// `compute_availability_with_isolation` special-cases
    /// (`anthropic-api-key` / `anthropic-oauth` / `openai-chatgpt`).
    ///
    /// These cells are shared by every clone so a successful write or delete
    /// cannot be undone by a later detached probe. They describe whether the
    /// credential for the route is currently present; `credential_sources`
    /// separately records which fixed authentication route the client was
    /// assembled with. In particular, storing an API key cannot hot-switch a
    /// client assembled for OAuth (or vice versa).
    pub(super) anthropic_has_api_key: Arc<std::sync::atomic::AtomicBool>,
    pub(super) anthropic_has_oauth: Arc<std::sync::atomic::AtomicBool>,
    pub(super) openai_chatgpt_available: Arc<std::sync::atomic::AtomicBool>,
    // Round-4 review finding (isolation boundary): mirrors the boot probe's
    // `cfg.isolated_credential_storage` (see `resolve_llm_stack`'s call to
    // `compute_availability_with_isolation` above) so a mid-session refresh
    // re-probes under the SAME isolation the boot used, instead of always
    // reading ambient machine env vars regardless of how this session booted.
    pub(super) isolated: bool,
}

impl FusionCatalogRefresher {
    /// Build a refresher over a shared availability map and an explicit list
    /// of KEYCHAIN-only profiles (no env-var fallback, no Anthropic/ChatGPT
    /// special-casing).
    ///
    /// The composition root builds the real thing with the full
    /// `provider_config::CredentialSource` list `resolve_llm_stack` already
    /// has; this is the constructor for callers OUTSIDE that root — which
    /// [`FusionCatalogRegistry`] now makes possible — that only know
    /// profile names.
    #[must_use]
    pub fn for_keychain_profiles(
        availability: Arc<std::sync::RwLock<std::collections::BTreeMap<String, bool>>>,
        credentials: Arc<CredentialManager>,
        profiles: &[&str],
    ) -> Self {
        Self {
            availability,
            credentials,
            credential_sources: profiles
                .iter()
                .map(|profile| provider_config::CredentialSource {
                    provider_id: llm_runtime::ProviderId::OpenAICompatible {
                        name: (*profile).to_string(),
                    },
                    profile_name: (*profile).to_string(),
                    credential_id: (*profile).to_string(),
                    env_var: None,
                    kind: provider_config::CredentialKind::Keychain,
                })
                .collect(),
            anthropic_has_api_key: fusion_route_flag(false),
            anthropic_has_oauth: fusion_route_flag(false),
            openai_chatgpt_available: fusion_route_flag(false),
            isolated: true,
            // This constructor is for callers OUTSIDE the composition root,
            // which hold the shared availability map but not the model
            // source's probe-completion cell, so the refresher it builds
            // gets a private one. That is inert rather than wrong: nothing
            // reads it, and re-arming is a pure recovery from a boot-probe
            // stall the composition root's own refresher (registered by
            // `resolve_llm_stack`, and invoked by the SAME
            // `refresh_fusion_catalog_after_credential_write` fan-out) still
            // performs on the real cell for the same credential write.
            availability_probe_completed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            mutation_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Re-probe every credential source (the SAME call `resolve_llm_stack`
    /// makes at boot, `provider_config::compute_availability_with_isolation`)
    /// and publish the result so the next `FusionCatalogModelSource::list()`
    /// sees it — this is what makes a mid-session `/connect`/`/login`
    /// credential write visible to Fusion without a process restart.
    ///
    /// The three special-cased route flags are not read from keychain here;
    /// credential mutation seams update their shared atomic cells before
    /// launching a refresh. Generic sources are re-probed from storage/env.
    pub async fn refresh(&self) {
        let epoch = self.current_mutation_epoch();
        self.refresh_inner(None, epoch).await;
    }

    /// Re-probe this runtime without making the caller wait on a credential
    /// broker.  The probe captures the current mutation epoch and therefore
    /// self-disqualifies if a newer write/delete lands before it commits.
    pub fn spawn_refresh(&self) {
        let refresher = self.clone();
        tokio::spawn(async move { refresher.refresh().await });
    }

    /// [`Self::refresh`] for a caller that knows WHICH credential it just
    /// persisted — the shape every production caller actually has.
    ///
    /// `credential_id` is matched against `CredentialSource::credential_id`
    /// (and, defensively, `profile_name`, because the TUI `/connect` seam
    /// keys its actions by the profile name the availability map uses); every
    /// matching profile is force-marked available regardless of what the
    /// re-probe answered. Round-5 review finding [5]: the re-probe bottoms
    /// out in `SecureStorage::contains`, whose `RuntimeFallbackStorage` impl
    /// answers `Ok(false)` — not `Err` — for `BackendUnavailable` /
    /// `PermissionDenied` / `Io`, so a degraded macOS credential broker makes
    /// a key that was just written successfully read back as absent. Without
    /// this the refresh triggered BY that write can conclude the provider is
    /// unavailable, which is the exact opposite of what it was called to do.
    pub async fn refresh_after_credential_write(&self, credential_id: &str) -> bool {
        // Round-12 rework: record the route BEFORE the re-probe, so the probe
        // resolves the three special-cased rows from the live state rather
        // than the boot snapshot — and so a later
        // `mark_credential_removed` of the OTHER Anthropic route can fall
        // back to this one instead of over-clearing the profile.
        let (epoch, compatibility) = self.publish_credential_established(credential_id).await;
        if compatibility == Some(true) {
            self.refresh_inner(Some(credential_id), epoch).await;
        }
        compatibility != Some(false)
    }

    /// Decide whether this catalog owns the named provider and, if so, whether
    /// its already-built client can use the newly stored credential.
    ///
    /// `None` makes an unrelated catalog neutral in the scoped fan-out.
    /// `Some(false)` means this catalog owns the provider but was assembled
    /// with another auth protocol, so callers must report that a restart is
    /// required rather than publishing false readiness.
    pub(super) fn credential_route_accepts_hot_write(&self, credential_id: &str) -> Option<bool> {
        match credential_id {
            // API-key routes resolve CredentialManager on every request, so a
            // cold API-key profile can adopt its first key without rebuilding
            // DefaultLlmClient. The assembled source is the proof that this
            // runtime actually chose that auth strategy.
            "anthropic" | "anthropic-api-key" => self
                .credential_sources
                .iter()
                .any(|source| source.profile_name == "anthropic")
                .then(|| {
                    self.credential_sources.iter().any(|source| {
                        source.profile_name == "anthropic"
                            && matches!(
                                source.credential_id.as_str(),
                                "anthropic" | "anthropic-api-key"
                            )
                    })
                }),
            // OAuth delegates are installed in MultiCredentialProvider only at
            // boot. An OAuth source proves that the delegate exists and can be
            // used again after logout/login; an API-key source cannot adopt a
            // newly persisted OAuth session without restart.
            "anthropic-oauth" => self
                .credential_sources
                .iter()
                .any(|source| source.profile_name == "anthropic")
                .then(|| {
                    self.credential_sources.iter().any(|source| {
                        source.profile_name == "anthropic"
                            && source.credential_id == "anthropic-oauth"
                    })
                }),
            // The ChatGPT preset exists even on a cold boot, so its source
            // alone does not prove MultiCredentialProvider received a live
            // delegate. The flag is deliberately conservative: after a
            // removal, a new login requires restart rather than claiming a
            // delegate this catalog cannot prove is still usable.
            "chatgpt" | "openai-chatgpt" => self
                .credential_sources
                .iter()
                .any(|source| source.profile_name == "openai-chatgpt")
                .then(|| {
                    self.openai_chatgpt_available
                        .load(std::sync::atomic::Ordering::Relaxed)
                        && self.credential_sources.iter().any(|source| {
                            source.profile_name == "openai-chatgpt"
                                && source.credential_id == "openai-chatgpt"
                        })
                }),
            // Generic API-key/Copilot routes also load CredentialManager per
            // request. Match both ids because UI seams publish profile names
            // while provider-config records the concrete credential id.
            other => self
                .credential_sources
                .iter()
                .any(|source| source.credential_id == other || source.profile_name == other)
                .then_some(true),
        }
    }

    /// Cheaply publish a successful credential write.
    ///
    /// Returns `false` only when this catalog owns the provider but its fixed
    /// client route cannot use the new credential without a restart. A catalog
    /// that does not contain the provider is a neutral success.
    pub async fn mark_credential_established(&self, credential_id: &str) -> bool {
        self.publish_credential_established(credential_id).await.1 != Some(false)
    }

    /// Publish a successful credential mutation and return the epoch attached
    /// to it.  The cheap map write is intentionally separate from the async
    /// broker probe so connect callers can publish readiness immediately.
    pub(super) async fn publish_credential_established(
        &self,
        credential_id: &str,
    ) -> (u64, Option<bool>) {
        let compatibility = self.credential_route_accepts_hot_write(credential_id);
        if compatibility.is_none() {
            return (self.current_mutation_epoch(), None);
        }
        // Treat the availability lock as the mutation boundary.  The epoch,
        // route flags and cheap map publication must move together: if the
        // epoch were bumped before acquiring this lock, a concurrent delete
        // could publish `false` and then the older write could resurrect the
        // profile after the delete returned.
        let Ok(mut guard) = self.availability.write() else {
            // A poisoned catalog is already unusable.  Still advance the
            // epoch so in-flight probes cannot commit against this mutation.
            if compatibility == Some(true) {
                self.note_route_credential_written(credential_id);
            }
            return (self.next_mutation_epoch(), compatibility);
        };
        // Raise the ROUTE flag first, so the re-derivation below — and every
        // later `refresh_inner`, which recomputes the three special-cased rows
        // from these same flags — sees the new credential.
        if compatibility == Some(true) {
            self.note_route_credential_written(credential_id);
        }

        // Every successful write has a known credential id.  Publish matching
        // generic sources immediately; the detached broker probe is only a
        // reconciliation pass and must not be the first point at which a
        // provider becomes visible.  Matching profile_name as well as
        // credential_id covers the keychain-profile constructor and the
        // provider-config source shape without doing any keychain I/O.
        if compatibility == Some(true) {
            for source in self.credential_sources.iter().filter(|source| {
                source.credential_id == credential_id || source.profile_name == credential_id
            }) {
                guard.insert(source.profile_name.clone(), true);
            }
        }

        match credential_id {
            // Same alias grouping as the removal arm. The helper gates live
            // credential flags by the single auth protocol this client wired.
            "anthropic" | "anthropic-api-key" | "anthropic-oauth" => {
                guard.insert("anthropic".to_string(), self.anthropic_route_available());
            }
            "chatgpt" | "openai-chatgpt" => {
                guard.insert(
                    "openai-chatgpt".to_string(),
                    self.openai_chatgpt_available
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
            }
            _ => {}
        }
        // Publish the epoch last while still holding the map lock. A detached
        // refresh samples the epoch without taking this lock; advancing it
        // before the flags/map would let that refresh observe the new epoch
        // with the old route state and later commit as if it were current.
        (self.next_mutation_epoch(), compatibility)
    }

    /// Publish a known credential removal without consulting storage.
    ///
    /// Generic profiles are re-derived from unaffected sources and ambient
    /// environment variables; special routes use their live presence flags
    /// gated by the fixed protocol recorded in `credential_sources`. The epoch
    /// is advanced after the map update so an older detached probe cannot
    /// resurrect the removed route.
    pub async fn mark_credential_removed(&self, credential_id: &str) {
        // A process may retain several independent runtime catalogs. A
        // credential for a provider absent from this one must not add a false
        // row or invalidate its in-flight probe.
        if self
            .credential_route_accepts_hot_write(credential_id)
            .is_none()
        {
            return;
        }
        // Treat the availability lock as the mutation boundary, just like
        // `publish_credential_established`: the delete's epoch, route flags
        // and map publication must be ordered as one mutation.  In-flight
        // probes check this epoch while holding the same lock before they
        // publish, so a late refresh cannot resurrect this delete.
        let Ok(mut guard) = self.availability.write() else {
            self.note_route_credential_removed(credential_id);
            self.next_mutation_epoch();
            return;
        };
        // (1) Lower the ROUTE flag first, so the re-derivation below — and
        //     every later `refresh_inner`, which recomputes the three
        //     special-cased rows from these same flags — sees the removal.
        //     Without this the next unrelated credential write republishes
        //     the boot value and resurrects the entry the delete cleared.
        self.note_route_credential_removed(credential_id);

        let affected: std::collections::BTreeSet<String> = self
            .credential_sources
            .iter()
            .filter(|source| {
                source.credential_id == credential_id || source.profile_name == credential_id
            })
            .map(|source| source.profile_name.clone())
            .collect();
        let rederived: Vec<(String, bool)> = affected
            .into_iter()
            .map(|profile| {
                let available = self
                    .credential_sources
                    .iter()
                    .filter(|source| source.profile_name == profile)
                    .any(|source| self.route_survives_removal(source, credential_id));
                (profile, available)
            })
            .collect();

        for (profile, available) in rederived {
            guard.insert(profile, available);
        }
        // Normalize the special aliases after the generic source pass. The
        // route flags record credential presence, while
        // `anthropic_route_available` additionally gates them by the one auth
        // protocol `assemble` wired into this client. Thus both Anthropic
        // credentials may exist without an OAuth logout falling back to an
        // API-key route the live client does not have.
        match credential_id {
            "anthropic" | "anthropic-api-key" | "anthropic-oauth" => {
                guard.insert("anthropic".to_string(), self.anthropic_route_available());
            }
            // ChatGPT gets no per-route OR because there is nothing to OR
            // WITH: `openai_chatgpt_available` is a single flag the engine
            // ORed over PAT-env / external-tokens-env / OAuth-session (see
            // `compute_availability_with_isolation`), and step (1) cleared
            // it. Both spellings of the slot share this arm — the OpenAI
            // OAuth handle deletes the minted key under `"chatgpt"`
            // (llm-runtime/src/oauth/openai/handle.rs), while the catalog
            // and `/connect` spell the same slot `"openai-chatgpt"`.
            "chatgpt" | "openai-chatgpt" => {
                guard.insert(
                    "openai-chatgpt".to_string(),
                    self.openai_chatgpt_available
                        .load(std::sync::atomic::Ordering::Relaxed),
                );
            }
            _ => {}
        }
        // As in the write path, advance only after every route/map update is
        // visible. This makes any refresh that overlapped the mutation carry
        // the preceding epoch and fail its commit check.
        self.next_mutation_epoch();
    }

    /// Is `source` still backed by something, given that `removed`'s KEYCHAIN
    /// entry has just been deleted?
    ///
    /// This is `compute_availability_with_isolation`'s per-source formula
    /// (provider-config/src/availability.rs:53-70) with the one substitution
    /// the caller established — `has_provider_key(removed) == false` — and no
    /// keychain read of its own.
    pub(super) fn route_survives_removal(
        &self,
        source: &provider_config::CredentialSource,
        removed: &str,
    ) -> bool {
        match source.credential_id.as_str() {
            "anthropic-api-key" => self
                .anthropic_has_api_key
                .load(std::sync::atomic::Ordering::Relaxed),
            "anthropic-oauth" => self
                .anthropic_has_oauth
                .load(std::sync::atomic::Ordering::Relaxed),
            "openai-chatgpt" => self
                .openai_chatgpt_available
                .load(std::sync::atomic::Ordering::Relaxed),
            _ if source.credential_id == removed || source.profile_name == removed => {
                // `keychain_has || env_set` with `keychain_has` now false.
                self.env_var_still_set(source.env_var.as_deref())
            }
            // A different credential id backs this profile as well and the
            // caller asserted nothing about it — assuming it is gone too is
            // exactly the over-clearing this rework exists to remove.
            _ => true,
        }
    }

    /// `!isolated && the variable exists` — the ambient half of
    /// `compute_availability_with_isolation`'s generic arm, honouring the same
    /// isolation boundary the boot probe used (round-4 finding).
    pub(super) fn env_var_still_set(&self, env_var: Option<&str>) -> bool {
        !self.isolated && env_var.is_some_and(|var| std::env::var(var).is_ok())
    }

    pub(super) fn next_mutation_epoch(&self) -> u64 {
        self.mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            .saturating_add(1)
    }

    pub(super) fn current_mutation_epoch(&self) -> u64 {
        self.mutation_epoch
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether the already-built Anthropic route has a live credential.
    ///
    /// `assemble` selects one auth protocol even when both credentials exist.
    /// Gate each presence flag by that selected source so logging out of an
    /// OAuth-built client cannot fall back to an unwired API key (or vice
    /// versa) and leave Fusion falsely ready.
    pub(super) fn anthropic_route_available(&self) -> bool {
        let api_key_route = self.credential_sources.iter().any(|source| {
            source.profile_name == "anthropic"
                && matches!(
                    source.credential_id.as_str(),
                    "anthropic" | "anthropic-api-key"
                )
        });
        let oauth_route = self.credential_sources.iter().any(|source| {
            source.profile_name == "anthropic" && source.credential_id == "anthropic-oauth"
        });
        (api_key_route
            && self
                .anthropic_has_api_key
                .load(std::sync::atomic::Ordering::Relaxed))
            || (oauth_route
                && self
                    .anthropic_has_oauth
                    .load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Record that `credential_id`'s route was just ESTABLISHED, for the three
    /// slots `compute_availability_with_isolation` resolves from flags rather
    /// than from a keychain read. See the `anthropic_has_api_key` field doc.
    ///
    /// # The bare `"anthropic"` spelling means an API-key write
    ///
    /// `secret::is_anthropic_api_key_id` is
    /// `matches!(id, "anthropic" | "anthropic-api-key")`, so a bare
    /// `"anthropic"` from a credential WRITE normally means an API key was
    /// stored (the bridge-server's `SetProviderCredential` arm and the TUI
    /// `/connect` key view). OAuth seams normalize the provider-level id to
    /// `"anthropic-oauth"` before publication. Keeping the bare spelling
    /// key-only makes this cheap path independent of broker reads and prevents
    /// an OAuth sign-in from inventing an API-key route.
    pub(super) fn note_route_credential_written(&self, credential_id: &str) {
        match credential_id {
            "anthropic-api-key" | "anthropic" => self
                .anthropic_has_api_key
                .store(true, std::sync::atomic::Ordering::Relaxed),
            "anthropic-oauth" => self
                .anthropic_has_oauth
                .store(true, std::sync::atomic::Ordering::Relaxed),
            "chatgpt" | "openai-chatgpt" => self
                .openai_chatgpt_available
                .store(true, std::sync::atomic::Ordering::Relaxed),
            _ => {}
        }
    }

    /// The removal twin of [`Self::note_route_credential_written`].
    ///
    /// Deleting the Anthropic API key does not necessarily END the API-key
    /// route: `DesktopConfig::api_key` is read from the `ANTHROPIC_API_KEY`
    /// environment variable (apps/cli/src/init.rs) and OUTRANKS the stored
    /// key, so on a non-isolated boot with that variable exported the route
    /// survives the delete. That is the same `keychain_has || env_set` rule
    /// [`Self::route_survives_removal`] applies to a generic profile, at the
    /// variable `provider_config::assemble` records for the Anthropic
    /// API-key source.
    pub(super) fn note_route_credential_removed(&self, credential_id: &str) {
        match credential_id {
            "anthropic" | "anthropic-api-key" => self.anthropic_has_api_key.store(
                self.env_var_still_set(Some("ANTHROPIC_API_KEY")),
                std::sync::atomic::Ordering::Relaxed,
            ),
            "anthropic-oauth" => self
                .anthropic_has_oauth
                .store(false, std::sync::atomic::Ordering::Relaxed),
            "chatgpt" | "openai-chatgpt" => self
                .openai_chatgpt_available
                .store(false, std::sync::atomic::Ordering::Relaxed),
            _ => {}
        }
    }

    /// Round-9 finding [3]: does this row's `available` verdict PROVE the
    /// credential backend answered?
    ///
    /// The re-arm gate below exists to withhold arming when the storage
    /// backend said nothing (a degraded broker answers `Ok(false)`, not
    /// `Err`), so it may only count rows whose availability was actually READ
    /// from storage. Two kinds of row are not:
    ///
    /// * the three ids `compute_availability_with_isolation` special-cases
    ///   (`anthropic-api-key` / `anthropic-oauth` / `openai-chatgpt`,
    ///   provider-config/src/availability.rs) resolve from
    ///   `anthropic_has_api_key` / `anthropic_has_oauth` /
    ///   `openai_chatgpt_available` — booleans frozen at construction that
    ///   never touch the keychain. On ANY install that booted with Anthropic
    ///   auth, one of them is permanently `true`, which made the gate
    ///   unconditionally satisfied and unable to do its documented job;
    /// * a generic profile whose `env_var` is set in a non-`isolated` process
    ///   is `keychain_has || env_set`, so `true` can come entirely from the
    ///   ambient environment while the keychain read returned nothing.
    ///
    /// Both answer `false` here. A `false` row is not "unavailable" — it is
    /// "this verdict is storage-INDEPENDENT", i.e. a degraded backend could
    /// not have changed it. `refresh_inner` uses that both ways (round-10
    /// finding N6): such a row cannot testify that the backend answered, but
    /// a probe made up ENTIRELY of such rows cannot have been corrupted by a
    /// degraded backend either, and is therefore authoritative on its own.
    /// Only the mixed case — some verdict did depend on a storage read —
    /// needs one of those reads to have come back `available`.
    pub(super) fn row_availability_came_from_storage(&self, credential_id: &str) -> bool {
        if matches!(
            credential_id,
            "anthropic-api-key" | "anthropic-oauth" | "openai-chatgpt"
        ) {
            return false;
        }
        if self.isolated {
            return true;
        }
        !self
            .credential_sources
            .iter()
            .filter(|source| source.credential_id == credential_id)
            .any(|source| {
                source
                    .env_var
                    .as_deref()
                    .is_some_and(|var| std::env::var(var).is_ok())
            })
    }

    /// Round-5 review finding [5]: this MERGES the re-probe into the live map
    /// instead of replacing it (`*guard = map`, the round-4 shape).
    ///
    /// The probe cannot distinguish "no credential" from "the credential
    /// backend is degraded": `provider_config::compute_availability_with_isolation`
    /// resolves every generic profile as
    /// `credentials.has_provider_key(id).await.unwrap_or(false)`, and
    /// `RuntimeFallbackStorage` already turned a `BackendUnavailable` into
    /// `Ok(false)` below that. A wholesale replace therefore let one degraded
    /// re-probe overwrite a known-good boot map with an all-`false` one and
    /// permanently empty Fusion's catalog for every non-anthropic profile
    /// (`TooFewModels{eligible:0}` on every subsequent `/fusion`), with
    /// nothing to repair it — strictly worse than the staleness the refresh
    /// was added to fix, and invisible because `DesktopRuntime.provider_availability`
    /// (the `/model` picker's copy) still showed those providers connected.
    ///
    /// Merge rule: a profile the probe reports AVAILABLE is published as
    /// available; a profile the probe reports UNAVAILABLE keeps whatever the
    /// map already held and is only inserted as `false` when the map had no
    /// entry for it at all. That is sound because a refresh is only ever
    /// triggered by a credential WRITE — no caller adds availability by
    /// deleting a credential, so a refresh has no business REMOVING any. A
    /// credential-REMOVAL path must NOT reuse this method — its `forced` loop
    /// would publish `true` for the very profile that just lost its
    /// credential. Removals go through [`Self::mark_credential_removed`]
    /// instead (round-12 finding [2]), which clears exactly the entries the
    /// removed credential backed and touches nothing else.
    pub(super) async fn refresh_inner(&self, just_written: Option<&str>, expected_epoch: u64) {
        let rows = provider_config::compute_availability_with_isolation(
            &self.credentials,
            &self.credential_sources,
            self.anthropic_has_api_key
                .load(std::sync::atomic::Ordering::Relaxed),
            self.anthropic_has_oauth
                .load(std::sync::atomic::Ordering::Relaxed),
            self.openai_chatgpt_available
                .load(std::sync::atomic::Ordering::Relaxed),
            self.isolated,
        )
        .await;
        let forced: Vec<String> = just_written
            .map(|id| {
                self.credential_sources
                    .iter()
                    .filter(|source| source.credential_id == id || source.profile_name == id)
                    .map(|source| source.profile_name.clone())
                    .collect()
            })
            .unwrap_or_default();
        // Round-7 finding [2]: re-arm the availability filter that a
        // timed-out BOOT probe disabled — but only on a probe whose result a
        // degraded credential backend could not have produced (see the
        // round-10 paragraph below for the exact rule; on the ordinary
        // install shape it reduces to "the probe observed an available row
        // it actually read from storage"). The probe cannot fail: it bottoms
        // out
        // in `has_provider_key(..).unwrap_or(false)` over a `SecureStorage`
        // whose runtime fallback already turned `BackendUnavailable` /
        // `PermissionDenied` / `Io` into `Ok(false)` (round-5 finding [5]),
        // so an all-`false` result is indistinguishable from a degraded
        // credential broker. Arming on THAT would publish an all-false map
        // as authoritative and empty Fusion's catalog for the rest of the
        // process (`TooFewModels{eligible:0}`) — strictly worse than the
        // over-broad catalog the fail-open leaves. `just_written`'s forced
        // entries are deliberately NOT counted here: they are asserted by
        // the caller, not observed by the probe.
        //
        // Round-9 finding [3]: only rows the probe actually READ can testify
        // that the backend answered — see `row_availability_came_from_storage`.
        // Counting every `available` row made this gate ALWAYS true on any
        // install that booted with an Anthropic key/OAuth or a ChatGPT
        // credential, i.e. inert exactly where its comment says it matters.
        //
        // Round-10 finding N6: rejecting those rows is right, but requiring
        // one of the REMAINING rows to be available turned the round-9 gate
        // into a false NEGATIVE on any install where NO row's verdict
        // depends on storage — an Anthropic-only/ChatGPT-only install (every
        // source is one of the three special-cased ids), or a non-isolated
        // process in which every generic profile carries a set `env_var`.
        // There the recovery this gate guards could never run at all, so one
        // 5s boot stall left `filter_fusion_catalog` failing open for the
        // whole process. The criterion the gate actually wants is not "a
        // storage-backed row said yes" but "nothing in this result could be
        // a lie from a degraded backend": when the probe contains no
        // storage-dependent row, the degraded-broker hypothesis cannot apply
        // to ANY row and the result is exactly what a boot probe that did
        // not stall would have published, so it is authoritative. When it
        // does contain storage-dependent rows, at least one of them must
        // have answered `available` — the round-9 rule, unchanged.
        let storage_dependent_rows: Vec<&provider_config::ProviderAvailability> = rows
            .iter()
            .filter(|row| self.row_availability_came_from_storage(&row.credential_id))
            .collect();
        let probe_result_is_authoritative = if storage_dependent_rows.is_empty() {
            true
        } else {
            storage_dependent_rows.iter().any(|row| row.available)
        };
        if let Ok(mut guard) = self.availability.write() {
            if self.current_mutation_epoch() != expected_epoch {
                return;
            }
            if probe_result_is_authoritative {
                self.availability_probe_completed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            for row in rows {
                if row.available {
                    guard.insert(row.profile_name, true);
                } else {
                    guard.entry(row.profile_name).or_insert(false);
                }
            }
            for profile in forced {
                guard.insert(profile, true);
            }
            guard
                .entry("anthropic".to_string())
                .or_insert(self.anthropic_route_available());
        }
    }
}

/// Credential catalog notifications shared only by explicitly composed runtimes.
/// Clones share a scope; constructing a new registry isolates its catalogs.
#[derive(Clone, Default)]
pub struct FusionCatalogRegistry {
    pub(super) refreshers: Arc<Mutex<Vec<FusionCatalogRefresher>>>,
}

/// Register a catalog with its owning credential scope.
pub fn register_fusion_catalog_refresher(
    registry: &FusionCatalogRegistry,
    refresher: FusionCatalogRefresher,
) {
    let mut guard = registry
        .refreshers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|entry| Arc::strong_count(&entry.availability) > 1);
    guard.push(refresher);
}

pub(super) fn live_fusion_catalog_refreshers(
    registry: &FusionCatalogRegistry,
) -> Vec<FusionCatalogRefresher> {
    let mut guard = registry
        .refreshers
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard.retain(|entry| Arc::strong_count(&entry.availability) > 1);
    guard.clone()
}

/// Cheap, ordered publication for a successful credential mutation.  This
/// performs no keychain/storage read: the known write/auth result is recorded
/// in each live catalog before a UI/connect caller reports readiness. Returns
/// `false` when a live client was booted with an incompatible fixed auth route;
/// the credential remains stored, but callers must require a restart instead
/// of publishing false readiness. Catalogs that do not contain the provider
/// are neutral. When several live catalogs contain the same provider, an
/// incompatible older route conservatively requires restart because this
/// scope contains catalogs sharing that credential binding.
pub async fn publish_fusion_catalog_credential(
    registry: &FusionCatalogRegistry,
    credential_id: &str,
) -> bool {
    publish_fusion_catalog_credential_to(live_fusion_catalog_refreshers(registry), credential_id)
        .await
}

pub(super) async fn publish_fusion_catalog_credential_to(
    refreshers: Vec<FusionCatalogRefresher>,
    credential_id: &str,
) -> bool {
    if refreshers.is_empty() {
        return true;
    }
    let mut routable = true;
    for refresher in refreshers {
        routable &= refresher.mark_credential_established(credential_id).await;
    }
    routable
}

/// Actionable copy for a credential that was persisted successfully but cannot
/// be adopted by this process's immutable provider route.
#[must_use]
pub fn fusion_credential_restart_required_message(credential_id: &str) -> String {
    format!(
        "Credential '{credential_id}' was saved, but this running session uses a different \
         authentication route. Restart LingXi to use the new credential."
    )
}

/// Start a scoped background availability re-probe.
/// Callers should publish first, then invoke this once; the epoch guard on each
/// refresher rejects a result that races a newer credential mutation.
pub fn spawn_fusion_catalog_refresh(registry: &FusionCatalogRegistry) {
    let refreshers = live_fusion_catalog_refreshers(registry);
    if refreshers.is_empty() {
        return;
    }
    tokio::spawn(async move {
        for refresher in refreshers {
            refresher.refresh().await;
        }
    });
}

/// Tell every live Fusion catalog filter in this credential scope that
/// `credential_id`'s credential was just written, so the next
/// `FusionCatalogModelSource::list()` sees it — the seam-agnostic half of
/// round-5 review finding [15]. A no-op in a process with no desktop runtime
/// (headless tests, the management subcommands).
pub async fn refresh_fusion_catalog_after_credential_write(
    registry: &FusionCatalogRegistry,
    credential_id: &str,
) -> bool {
    let routable = publish_fusion_catalog_credential(registry, credential_id).await;
    if routable {
        spawn_fusion_catalog_refresh(registry);
    }
    routable
}

/// The removal twin of [`refresh_fusion_catalog_after_credential_write`]:
/// tell every live Fusion catalog filter in this credential scope that
/// `credential_id`'s credential is GONE, so the next
/// `FusionCatalogModelSource::list()` stops offering the models it backed.
///
/// Round-12 finding [2]: without this, deleting a provider key (or signing
/// out) mid-session left `availability[profile] = true` for the rest of the
/// process — a re-probe cannot lower it by design — so a `/fusion` preset
/// could auto-select the provider, reserve budget for it, and only discover
/// the credential was gone as an `LlmError::Authentication` at request time,
/// instead of the §4 preflight excluding it.
///
/// Unlike the write twin this does no keychain I/O — it re-derives the
/// affected profiles from the credential sources, the shared route flags and
/// `std::env::var` (see [`FusionCatalogRefresher::mark_credential_removed`])
/// — so callers do not need to bound it with
/// `crate::desktop::boot::refresh_fusion_catalog_bounded`. A no-op in a process with
/// no desktop runtime.
pub async fn refresh_fusion_catalog_after_credential_delete(
    registry: &FusionCatalogRegistry,
    credential_id: &str,
) {
    for refresher in live_fusion_catalog_refreshers(registry) {
        refresher.mark_credential_removed(credential_id).await;
    }
}

/// Cheap publication for `/login`: raise the live route flag and its map entry
/// without a credential-backend probe. The canonical OAuth id is checked
/// against the protocol already wired into each runtime; a provider absent
/// from a catalog is neutral, while an incompatible route requires restart.
pub(super) async fn note_fusion_catalog_credential_route(
    registry: &FusionCatalogRegistry,
    credential_id: &str,
) -> bool {
    publish_fusion_catalog_credential(registry, credential_id).await
}

/// Wraps the `/connect <provider>` generic-API-key seam so a successful
/// credential write also refreshes [`FusionCatalogRefresher`] (round-4
/// review finding [8]) — kept entirely on the desktop side rather than
/// touching `commands/core`'s trait or `EngineCredentialWriter`, because
/// refreshing Fusion's catalog is a desktop-engine-specific consequence of a
/// credential write, not part of the connect contract itself. Anthropic and
/// ChatGPT connects are not routed through this seam (they have their own
/// drivers) — round-5 review finding [15] wraps those too
/// ([`FusionCatalogRefreshingChatGptConnect`],
/// [`FusionCatalogRefreshingOAuthConnect`]), and
/// the cheap publisher raises availability only when the already-built client
/// route can use the new credential. An incompatible protocol is persisted but
/// reported as restart-required instead of falsely connected.
pub(super) struct FusionCatalogRefreshingCredentialWriter {
    pub(super) inner: Arc<dyn command_api::builtins::ConnectCredentialWriter>,
    pub(super) refresher: FusionCatalogRefresher,
}

#[async_trait::async_trait]
impl command_api::builtins::ConnectCredentialWriter for FusionCatalogRefreshingCredentialWriter {
    async fn prompt_and_store_key(
        &self,
        credential_id: &str,
    ) -> Result<(), command_api::builtins::ConnectError> {
        self.inner.prompt_and_store_key(credential_id).await?;
        let mutation_id = if credential_id == "anthropic" {
            "anthropic-api-key"
        } else {
            credential_id
        };
        if !self
            .refresher
            .mark_credential_established(mutation_id)
            .await
        {
            return Err(command_api::builtins::ConnectError::Network(
                fusion_credential_restart_required_message(mutation_id),
            ));
        }
        self.refresher.spawn_refresh();
        Ok(())
    }
}

/// See [`FusionCatalogRefreshingCredentialWriter`] — the same wrapping for
/// the Copilot device-flow seam, whose `poll_to_completion` is where a
/// GitHub Copilot token is actually persisted
/// (`EngineCopilotConnect::poll_to_completion`, `connect.rs`) — the exact
/// path the finding [8] scenario names.
pub(super) struct FusionCatalogRefreshingCopilotConnect {
    pub(super) inner: Arc<dyn command_api::builtins::CopilotConnectDriver>,
    pub(super) refresher: FusionCatalogRefresher,
}

#[async_trait::async_trait]
impl command_api::builtins::CopilotConnectDriver for FusionCatalogRefreshingCopilotConnect {
    async fn begin(
        &self,
        domain: Option<&str>,
    ) -> Result<command_api::builtins::CopilotConnectStep, command_api::builtins::ConnectError>
    {
        self.inner.begin(domain).await
    }

    async fn poll_to_completion(
        &self,
        step: &command_api::builtins::CopilotConnectStep,
    ) -> Result<(), command_api::builtins::ConnectError> {
        self.inner.poll_to_completion(step).await?;
        // The Copilot device flow persists under the `github-copilot`
        // credential id (`EngineCopilotConnect::poll_to_completion`); naming
        // it keeps the degraded-backend guard of
        // `refresh_after_credential_write` in play here too (finding [5]).
        if !self
            .refresher
            .mark_credential_established("github-copilot")
            .await
        {
            return Err(command_api::builtins::ConnectError::Network(
                fusion_credential_restart_required_message("github-copilot"),
            ));
        }
        self.refresher.spawn_refresh();
        Ok(())
    }
}

/// See [`FusionCatalogRefreshingCredentialWriter`] — the same wrapping for
/// the ChatGPT-subscription OAuth seam (`/connect chatgpt`), which persists
/// its credential inside `llm_runtime::oauth::openai`'s handle rather than
/// through `ConnectCredentialWriter`. Round-5 review finding [15] class
/// sweep: this was the one harness-runtime::desktop `/connect` driver round 4 left
/// unwrapped, so a ChatGPT sign-in stayed invisible to Fusion for the rest of
/// the process.
pub(super) struct FusionCatalogRefreshingChatGptConnect {
    pub(super) inner: Arc<dyn command_api::builtins::ChatGptConnectDriver>,
    pub(super) refresher: FusionCatalogRefresher,
}

#[async_trait::async_trait]
impl command_api::builtins::ChatGptConnectDriver for FusionCatalogRefreshingChatGptConnect {
    async fn connect(&self) -> Result<String, command_api::builtins::ConnectError> {
        let message = self.inner.connect().await?;
        if !self
            .refresher
            .mark_credential_established("openai-chatgpt")
            .await
        {
            return Err(command_api::builtins::ConnectError::Network(
                fusion_credential_restart_required_message("openai-chatgpt"),
            ));
        }
        self.refresher.spawn_refresh();
        Ok(message)
    }
}

/// Round-12 finding [2], class sweep: the same wrapping for the SIGN-OUT
/// direction, which had no wrapper at all.
///
/// Every sign-IN seam refreshes Fusion's availability map, and
/// `FusionCatalogRefresher`'s merge rule can never lower a `true` (see
/// [`FusionCatalogRefresher::refresh_inner`]) — so once a process published
/// `anthropic: true`, `/logout` (`command_api::builtins::LogoutHandler`, the TUI) and
/// the bridge-server's `ClientCommand::Logout` both left Fusion offering
/// Anthropic models the session could no longer authenticate, for the rest of
/// the process. Wrapping the ONE `Arc<dyn AuthHandle>` the whole desktop
/// runtime shares covers every consumer of it at once rather than asking each
/// surface to remember the call.
///
/// `login` notes the OAUTH route it just established (round-12 rework) and
/// otherwise delegates untouched. `FusionCatalogRefreshingOAuthConnect` only
/// covers the `/connect` picker; `/login` (`command_api::builtins::LoginHandler`) and
/// the bridge-server's `ClientCommand::Login` drive this handle directly, and
/// if their sign-in went unrecorded a later "delete the Anthropic API key"
/// would fall back to a stale `anthropic_has_oauth == false` and clear a
/// profile the OAuth session still routes. The note is a single atomic store
/// — no keychain I/O — so the login path keeps its current cost; publishing
/// the map entry stays the job of the write fan-out, and a rollback inside a
/// failed login goes through `logout` below.
pub(super) struct FusionCatalogClearingAuth {
    pub(super) catalog_registry: FusionCatalogRegistry,
    pub(super) inner: Arc<dyn AuthHandle>,
}

#[async_trait::async_trait]
impl AuthHandle for FusionCatalogClearingAuth {
    async fn login(&self) -> Result<platform_api::auth::LoginInfo, platform_api::auth::AuthError> {
        let info = self.inner.login().await?;
        if !note_fusion_catalog_credential_route(&self.catalog_registry, "anthropic-oauth").await {
            return Err(platform_api::auth::AuthError::ServerError(
                fusion_credential_restart_required_message("anthropic-oauth"),
            ));
        }
        Ok(info)
    }

    async fn logout(&self) -> Result<(), platform_api::auth::AuthError> {
        self.inner.logout().await?;
        refresh_fusion_catalog_after_credential_delete(&self.catalog_registry, "anthropic-oauth")
            .await;
        Ok(())
    }

    async fn current_user(&self) -> Option<platform_api::auth::LoginInfo> {
        self.inner.current_user().await
    }
}

/// See [`FusionCatalogRefreshingCredentialWriter`] — the same wrapping for
/// the unified OAuth sign-in seam the TUI `/connect` picker uses (Anthropic
/// Pro/Max, OpenAI ChatGPT). Round-5 review finding [15] class sweep: an
/// OAuth sign-in persists a credential exactly like an API-key write does,
/// and `EngineOAuthConnect` was not one of the wrapped drivers.
pub(super) struct FusionCatalogRefreshingOAuthConnect {
    pub(super) inner: Arc<dyn command_api::builtins::OAuthConnectDriver>,
    pub(super) refresher: FusionCatalogRefresher,
}

#[async_trait::async_trait]
impl command_api::builtins::OAuthConnectDriver for FusionCatalogRefreshingOAuthConnect {
    async fn login(
        &self,
        provider_id: &str,
    ) -> Result<String, command_api::builtins::ConnectError> {
        let message = self.inner.login(provider_id).await?;
        // Round-12 rework: this is the ONE seam where a bare `"anthropic"`
        // means the OAUTH route — `EngineOAuthConnect::login` maps it to
        // `AuthHandle::login` (connect.rs), not to a stored API key. Every
        // other write seam that says `"anthropic"` stored an API key
        // (`secret::is_anthropic_api_key_id` matches the bare spelling), so
        // notify under the unambiguous credential id here and let
        // `note_route_credential_written` keep the bare spelling meaning
        // "API key" everywhere else. Without this, signing in with OAuth
        // would raise the API-KEY flag and a later "delete the API key" would
        // leave Anthropic in Fusion's catalog on the strength of a route that
        // never existed.
        let credential_id = match provider_id {
            "anthropic" => "anthropic-oauth",
            other => other,
        };
        if !self
            .refresher
            .mark_credential_established(credential_id)
            .await
        {
            return Err(command_api::builtins::ConnectError::Network(
                fusion_credential_restart_required_message(credential_id),
            ));
        }
        self.refresher.spawn_refresh();
        Ok(message)
    }
}

/// `fusion::FusionPriceBook` over the session's `cost::PricingCatalog` — the
/// SAME catalog `CostTracker` bills from (see the WP1/F001/G003 comment at
/// its construction site). Before this adapter existed, `FusionOrchestrator`
/// was always built with the `()` price book (`rates_for` always `None`), so
/// `budget::quote`'s `model_peak` hard-rejected every token-billed model
/// under a session `--max-budget` (`InvalidConfiguration("... has no
/// price")`) and, without a cap, every reservation quoted $0 — Fusion's
/// hard-budget invariant (design §4) was wired to nothing.
///
/// `orchestrator::cost_wiring::model_ref_from_string` is the SAME
/// profile+bare-model → `ModelRef` resolution the main turn loop uses for its
/// own `record_api_response_v2` calls, so a Fusion panel/analyst/synth model
/// prices exactly like the corresponding main-loop call would.
pub(super) struct DesktopFusionPriceBook {
    pub(super) catalog: Arc<cost::PricingCatalog>,
    /// Round-7 finding [1]: whether this process assembles its Anthropic
    /// requests with 1-HOUR prompt-cache TTLs, so cache-CREATION tokens are
    /// billed at the catalog's `TokenClass::CacheWrite1h` rate instead of
    /// the 5-minute `TokenClass::CacheWrite` one. See
    /// [`prompt_cache_write_ttl_1h_enabled`].
    pub(super) cache_write_ttl_1h: bool,
}

/// The 1-hour prompt-cache TTL gate, read exactly where the money is priced.
///
/// `llm_runtime::service::ApiService::should_1h_cache_ttl`
/// (llm-runtime/src/service.rs:1090) reads this same variable through the same
/// truthy set (`1|true|yes|on`) and, when it is set, stamps `ttl_1h` on the
/// system cache blocks of EVERY request `build_request` assembles — which
/// includes Fusion's panel turns (`stream_forced_with_opts`) and its
/// analyst/synthesizer side queries. Anthropic bills those cache-creation
/// tokens at its 1-hour rate (~1.6x the 5-minute rate; `cost/src/pricing.rs`
/// derives the class for every Anthropic tier, e.g. sonnet 3_750 -> 6_000).
///
/// `orchestrator::conversation::hooks`'s model-switch cache-write estimator
/// already consults this same variable to pick between
/// `TokenClass::CacheWrite1h` and `TokenClass::CacheWrite` for an aggregate
/// cache-write token count it likewise has no 1h/5m split for — this mirrors
/// that precedent rather than inventing a second rule.
pub(super) fn prompt_cache_write_ttl_1h_enabled() -> bool {
    platform_api::env::is_env_truthy(std::env::var("ENABLE_PROMPT_CACHING_1H").ok().as_deref())
}

impl DesktopFusionPriceBook {
    /// Production constructor: resolve the session's prompt-cache TTL gate
    /// once, from the same env var `llm_runtime` reads when it builds the
    /// requests this book prices.
    pub(super) fn new(catalog: Arc<cost::PricingCatalog>) -> Self {
        Self {
            catalog,
            cache_write_ttl_1h: prompt_cache_write_ttl_1h_enabled(),
        }
    }

    /// Test constructor with the TTL gate stated explicitly, so a test that
    /// does not care about the gate never has to touch process env (and can
    /// run concurrently with the three tests that do).
    #[cfg(test)]
    pub(super) fn with_ttl_gate(
        catalog: Arc<cost::PricingCatalog>,
        cache_write_ttl_1h: bool,
    ) -> Self {
        Self {
            catalog,
            cache_write_ttl_1h,
        }
    }
}

impl fusion::FusionPriceBook for DesktopFusionPriceBook {
    fn rates_for(&self, profile: &str, model: &str) -> Option<fusion::ModelRates> {
        let model_ref = orchestrator::cost_wiring::model_ref_from_string(model, Some(profile));
        let (pricing, _resolution) = self.catalog.resolve(&model_ref).ok()?;
        let input = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::Input)?
            .nano_usd_per_token;
        let output = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::Output)?
            .nano_usd_per_token;
        // Cache-read/-write rates default to 0 rather than `?`-propagating a
        // `None`: a model with real Input/Output rates but no CacheRead /
        // CacheWrite entry must stay token-priced (only the cache premium is
        // unrecovered), never flip to fully unpriced and hard-reject under a
        // session `--max-budget` (`model_peak`'s `InvalidConfiguration`
        // branch). `cost::calculator::CostCalculator` — the SAME catalog the
        // main turn loop bills from — already treats a missing rate for a
        // class it iterates as "that class contributes 0", so this mirrors
        // it rather than diverging.
        let cache_read = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::CacheRead)
            .map_or(0, |rate| rate.nano_usd_per_token);
        // Round-7 finding [1]: pick the cache-WRITE rate that matches the TTL
        // this process actually stamps on its cache blocks. With
        // `ENABLE_PROMPT_CACHING_1H` armed, `ApiService::build_request` marks
        // the system cache blocks of every Fusion request `ttl_1h`, and
        // Anthropic bills those creation tokens at ~1.6x the 5-minute rate —
        // a rate the SAME catalog already carries as
        // `TokenClass::CacheWrite1h` and that the main turn loop bills
        // through (`orchestrator::cost_wiring` splits
        // `provider_metadata./cache_creation/ephemeral_1h_input_tokens` into
        // `TokenUsage::cache_write_1h`, which `CostCalculator` prices through
        // that class). Fusion has no such split to price from: both seams
        // that feed it flatten the two buckets into one total
        // (`agent/src/handle.rs`'s `cache_creation_input_tokens: bt.cache_write`
        // and `sidequery`'s `cache_write_1h: 0`), so `FusionUsage` carries a
        // single `cache_write_tokens` figure. Given only that total, billing
        // it at the 5-minute rate under an armed 1h gate is a SILENT 37.5%
        // under-charge on the money path (design §4's hard-budget ceiling
        // then sits below what the provider actually charged); billing it at
        // the 1h rate is at worst high by the residual 5-minute
        // message-level breakpoint (service.rs's `CacheControl::Ephemeral`
        // on the last message block), which errs toward reserving/settling
        // MORE than was spent — the safe direction for a budget ceiling.
        // With the gate off (the shipped default — service.rs documents the
        // feature as deliberately dormant, no settings.json route) this is
        // byte-identical to the previous behaviour and exactly correct.
        //
        // The `.or_else` fallback matters: `provider-config::cost_translate`
        // never inserts a `CacheWrite1h` class, so every models.dev /
        // OpenRouter entry keeps its 5-minute rate here instead of dropping
        // to 0 the moment the gate is armed.
        let cache_write_class = if self.cache_write_ttl_1h {
            cost::pricing::TokenClass::CacheWrite1h
        } else {
            cost::pricing::TokenClass::CacheWrite
        };
        let cache_write = pricing
            .token_rates
            .get(&cache_write_class)
            .or_else(|| {
                pricing
                    .token_rates
                    .get(&cost::pricing::TokenClass::CacheWrite)
            })
            .map_or(0, |rate| rate.nano_usd_per_token);
        // Finding [1]: `provider-config::cost_translate::model_pricing_from_token_pricing`
        // now ALWAYS inserts a `ReasoningOutput` rate — a model that publishes
        // a real, separate reasoning price (DeepSeek / a handful of
        // OpenRouter rows) keeps it; everyone else (most OpenAI / Gemini /
        // Copilot / … rows, which bill reasoning tokens at the plain output
        // rate) gets the output rate as the class's value. That fix lives at
        // the shared catalog layer so both this Fusion price book and the
        // main turn loop's `cost::CostCalculator` (which iterates only the
        // classes present in `token_rates`) recover the bucket identically.
        // The `map_or(output, …)` here is a second line of defense for any
        // pricing entry that reaches this book without going through
        // `cost_translate` (e.g. a future direct catalog entry) — it must
        // still stay token-priced, never flip to fully unpriced.
        let reasoning = pricing
            .token_rates
            .get(&cost::pricing::TokenClass::ReasoningOutput)
            .map_or(output, |rate| rate.nano_usd_per_token);
        Some(fusion::ModelRates {
            input_nano_usd_per_token: input,
            output_nano_usd_per_token: output,
            // The pricing catalog has no flat per-request rate today (only
            // per-token + the separate web-search non-token unit); Fusion's
            // quote/settlement formulas treat a zero per-request rate as "no
            // flat fee", not "unpriced" — token rates alone still gate the
            // hard-budget preflight correctly.
            per_request_nano_usd: 0,
            cache_read_nano_usd_per_token: cache_read,
            cache_write_nano_usd_per_token: cache_write,
            reasoning_nano_usd_per_token: reasoning,
            // Round-7 finding [1] residual: with the gate armed, the single
            // rate above is right for the system cache blocks
            // (`service.rs` stamps `ttl_1h` on them) and wrong for the
            // residual last-message breakpoint, which stays 5-minute
            // (`CacheControl::Ephemeral`) — and Fusion carries no split to
            // tell the two apart. Report the rate as approximated so
            // `price_realized_usage` refuses to claim `estimated: false`
            // over it. With the gate off (the shipped default) every cache
            // block is 5-minute and the rate is exact.
            cache_write_rate_is_ttl_approximated: self.cache_write_ttl_1h,
        })
    }
}

/// Resolve the effective Fusion settings for one snapshot (F007).
///
/// Routes through [`load_effective_settings_for_config`] — the SAME
/// managed/CLI/scoped-source-aware loader `build()` uses for every other
/// setting — instead of a bare `Settings::load`, so a managed-policy
/// `fusion.enabled=false`/`allowedProfiles`/`allowCrossProviderForAgent=false`
/// and `--settings '{"fusion":…}'` are honored (previously ignored: see the
/// F007 finding).
pub(super) fn desktop_fusion_runtime_config(
    cfg: &DesktopConfig,
) -> Result<fusion::FusionRuntimeConfig, platform_api::FusionError> {
    // Managed policy is intentionally read at the call boundary.  Keeping a
    // boot-time `Vec<String>` here makes `FusionConfigSource::load()` and the
    // executor's preflight disagree with the next on-disk policy edit.
    let managed_raw_tiers = managed_settings_raw_tiers_sync();
    let effective =
        load_effective_settings_for_config(cfg, &managed_raw_tiers).ok_or_else(|| {
            platform_api::FusionError::InvalidConfiguration("settings failed to load".into())
        })?;
    match effective.settings.fusion {
        Some(settings) => fusion::FusionRuntimeConfig::from_settings(&settings),
        None => Ok(fusion::FusionRuntimeConfig::defaults()),
    }
}

/// F007: reload point handed to `FusionOrchestrator` — `load()` re-resolves
/// the effective settings on every call instead of a config frozen at
/// construction, so a settings-file edit or the design's §11 kill switch
/// (`fusion.enabled=false`) takes effect on the NEXT run, not the next
/// process restart.
pub(super) struct DesktopFusionConfigSource {
    pub(super) cfg: DesktopConfig,
}

impl fusion::FusionConfigSource for DesktopFusionConfigSource {
    fn load(&self) -> Result<fusion::FusionRuntimeConfig, platform_api::FusionError> {
        desktop_fusion_runtime_config(&self.cfg)
    }
}

/// Finding [14]: wraps the constructed live `FusionOrchestrator` so a
/// boot-time-invalid `fusion.*` value does NOT pin `RejectedFusionExecutor`
/// for the rest of the process. `preflight_error()` re-validates FRESH on
/// every call — mirroring how `FusionOrchestrator::run` /
/// `agent_surface` / `workflow_fusion_call_cap` already reload the config
/// per call (F007) — instead of freezing the first boot-time error, so a
/// user who fixes and saves the settings file recovers within the session
/// exactly as the F007 doc comment promises ("takes effect on the NEXT run,
/// not the next process restart"), rather than only after a restart.
pub(super) struct DesktopFusionExecutor {
    pub(super) inner: Arc<fusion::FusionOrchestrator>,
    pub(super) cfg: DesktopConfig,
}

#[async_trait::async_trait]
impl platform_api::FusionExecutor for DesktopFusionExecutor {
    fn prepare(
        self: Arc<Self>,
        submission: platform_api::FusionSubmission,
    ) -> Result<platform_api::PreparedFusionRun, platform_api::FusionError> {
        Arc::clone(&self.inner).prepare(submission)
    }

    fn effective_timeout_ms(&self) -> Option<u64> {
        self.inner.effective_timeout_ms()
    }

    fn agent_surface(&self) -> platform_api::FusionAgentSurface {
        self.inner.agent_surface()
    }

    fn preflight_error(&self) -> Option<platform_api::FusionError> {
        desktop_fusion_runtime_config(&self.cfg).err()
    }

    fn resolve_parent_profile(
        &self,
        parent_model: &str,
        explicit_profile: Option<&str>,
    ) -> Option<String> {
        self.inner
            .resolve_parent_profile(parent_model, explicit_profile)
    }

    fn workflow_fusion_call_cap(&self) -> u32 {
        self.inner.workflow_fusion_call_cap()
    }

    fn workflow_batch_concurrency(&self) -> usize {
        self.inner.workflow_batch_concurrency()
    }
}

/// Install one shared physical-attempt host only after durable boot has
/// published its output scope. The registry validates captured session
/// hydration again during prepare; this factory grants no dispatch authority.
pub(super) fn desktop_fusion_attempts(
    service: Arc<llm_runtime::ApiService>,
    budget: Arc<cost::BudgetEnforcer>,
    tracker: Arc<cost::CostTracker>,
    pricing: Arc<cost::PricingCatalog>,
    outputs: Arc<dyn platform_api::WorkflowOutputScopes>,
) -> Arc<fusion_attempts::DesktopFusionAttempts> {
    let attempts = fusion_attempts::DesktopFusionAttempts::new(
        service.clone(),
        budget,
        tracker,
        pricing,
        outputs,
    );
    service.set_model_attempt_hooks(attempts.clone());
    attempts
}
