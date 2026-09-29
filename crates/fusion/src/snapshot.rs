//! Immutable Fusion preparation snapshots.

use crate::budget::{CapturedPriceBook, FusionPriceBook};
use crate::config::FusionRuntimeConfig;
use crate::model_resolver::{canonical_key, route_key, CatalogModel, ModelLimits, ModelSource};
use lingxi_core::host::FusionError;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Source revision plus a deterministic content digest. Both are retained so
/// a host revision can invalidate quickly while content protects generic
/// ModelSource implementations that only expose the default revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CatalogRevision {
    /// Host-provided monotonic source epoch.
    pub source: u64,
    /// Digest of the filtered route content captured for this preparation.
    pub content: u64,
}

/// One immutable catalog read used by a prepared Fusion run.
#[derive(Clone, PartialEq, Eq)]
pub struct CatalogSnapshot {
    rows: Arc<Vec<CatalogModel>>,
    revision: CatalogRevision,
    canonical_keys: Arc<BTreeMap<String, String>>,
}

impl Debug for CatalogSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CatalogSnapshot")
            .field("rows", &self.rows.len())
            .field("revision", &self.revision)
            .finish()
    }
}

impl CatalogSnapshot {
    /// Capture a stable source view without a TTL or mutable cache.
    ///
    /// # Errors
    ///
    /// Returns [`FusionError::InvalidConfiguration`] when the source keeps
    /// mutating across all bounded capture attempts.
    pub fn capture(source: &dyn ModelSource) -> Result<Self, FusionError> {
        for _ in 0..4 {
            if let Some(snapshot) = Self::try_capture_stable(source) {
                return Ok(snapshot);
            }
        }
        Err(FusionError::InvalidConfiguration(
            "fusion catalog changed during snapshot capture".into(),
        ))
    }

    /// Attempt one coherent source capture. The monotonic revision brackets
    /// two full reads: the epoch catches remove/re-add ABA mutations while the
    /// row equality protects generic sources whose revision stays at zero.
    #[must_use]
    pub(crate) fn try_capture_stable(source: &dyn ModelSource) -> Option<Self> {
        let revision_before = source.revision();
        let rows_before = source.list();
        let rows_after = source.list();
        let revision_after = source.revision();
        if revision_before != revision_after || rows_before != rows_after {
            return None;
        }
        Some(Self::from_rows(rows_after, revision_after))
    }

    fn from_rows(rows: Vec<CatalogModel>, source_revision: u64) -> Self {
        let revision = CatalogRevision {
            source: source_revision,
            content: content_digest(&rows),
        };
        let canonical_keys = rows
            .iter()
            .map(|row| {
                (
                    route_key(&row.profile, &row.model),
                    canonical_key(&row.model),
                )
            })
            .collect();
        Self {
            rows: Arc::new(rows),
            revision,
            canonical_keys: Arc::new(canonical_keys),
        }
    }

    /// Rows captured at prepare time.
    #[must_use]
    pub fn rows(&self) -> &[CatalogModel] {
        self.rows.as_slice()
    }

    /// Revision/content identity of this snapshot.
    #[must_use]
    pub const fn revision(&self) -> CatalogRevision {
        self.revision
    }

    /// Exact route limits captured for a profile/model pair.
    #[must_use]
    pub fn limits_for(&self, profile: &str, model: &str) -> Option<ModelLimits> {
        self.row_for(profile, model).map(|row| row.limits)
    }

    /// Exact route row captured at prepare time.
    #[must_use]
    pub fn row_for(&self, profile: &str, model: &str) -> Option<&CatalogModel> {
        self.rows
            .iter()
            .find(|row| row.profile == profile && row.model == model)
    }

    /// Canonical underlying-model identity captured for a route.
    #[must_use]
    pub fn canonical_key_for(&self, profile: &str, model: &str) -> String {
        self.canonical_keys
            .get(&route_key(profile, model))
            .cloned()
            .unwrap_or_else(|| canonical_key(model))
    }

    /// Whether the snapshot contains the exact route.
    #[must_use]
    pub fn contains_route(&self, profile: &str, model: &str) -> bool {
        self.limits_for(profile, model).is_some()
    }

    /// Capture immutable prices for every catalog route plus selected routes
    /// not present in the filtered catalog (normally the parent route).
    #[must_use]
    pub fn capture_prices(
        &self,
        prices: &dyn FusionPriceBook,
        extra_routes: &[(String, String)],
    ) -> CapturedPriceBook {
        let mut routes = BTreeSet::new();
        for row in self.rows() {
            routes.insert((row.profile.clone(), row.model.clone()));
        }
        routes.extend(extra_routes.iter().cloned());
        CapturedPriceBook::capture(prices, routes)
    }
}

impl ModelSource for CatalogSnapshot {
    fn list(&self) -> Vec<CatalogModel> {
        self.rows.as_ref().clone()
    }

    fn revision(&self) -> u64 {
        self.revision.source
    }
}

/// Complete immutable runtime input captured before activation.
#[derive(Clone, Debug)]
pub struct FusionRuntimeSnapshot {
    /// Resolved Fusion settings.
    pub config: FusionRuntimeConfig,
    /// Catalog and route capacities.
    pub catalog: CatalogSnapshot,
    /// Captured per-route prices.
    pub prices: CapturedPriceBook,
    /// Digest of the effective config values.
    pub config_content: u64,
}

impl FusionRuntimeSnapshot {
    /// Construct a runtime snapshot from already-captured catalog and prices.
    #[must_use]
    pub fn new(
        config: FusionRuntimeConfig,
        catalog: CatalogSnapshot,
        prices: CapturedPriceBook,
    ) -> Self {
        let config_content = debug_digest(&config);
        Self {
            config,
            catalog,
            prices,
            config_content,
        }
    }
}

fn content_digest(rows: &[CatalogModel]) -> u64 {
    let mut values: Vec<String> = rows.iter().map(|row| format!("{row:?}")).collect();
    values.sort();
    debug_digest(&values)
}

fn debug_digest<T: Debug>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{value:?}").hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::ModelRates;
    use crate::model_resolver::ModelSource;
    use lingxi_core::host::FusionModelHints;
    use std::sync::Mutex;

    struct MutableCatalog {
        rows: Mutex<Vec<CatalogModel>>,
        revision: Mutex<u64>,
    }

    impl ModelSource for MutableCatalog {
        fn list(&self) -> Vec<CatalogModel> {
            self.rows.lock().unwrap().clone()
        }

        fn revision(&self) -> u64 {
            *self.revision.lock().unwrap()
        }
    }

    struct MutablePrices {
        rates: Mutex<Option<ModelRates>>,
    }

    impl FusionPriceBook for MutablePrices {
        fn rates_for(&self, _profile: &str, _model: &str) -> Option<ModelRates> {
            *self.rates.lock().unwrap()
        }
    }

    fn row(model: &str) -> CatalogModel {
        CatalogModel {
            profile: "profile".into(),
            model: model.into(),
            hints: FusionModelHints::default(),
            structured_output: true,
            limits: ModelLimits {
                context_window_tokens: Some(16_000),
                max_input_tokens: Some(12_000),
                max_output_tokens: Some(2_000),
            },
        }
    }

    fn rates(input: u64) -> ModelRates {
        ModelRates {
            input_nano_usd_per_token: input,
            output_nano_usd_per_token: 1,
            per_request_nano_usd: 0,
            cache_read_nano_usd_per_token: 0,
            cache_write_nano_usd_per_token: 0,
            reasoning_nano_usd_per_token: 0,
            cache_write_rate_is_ttl_approximated: false,
        }
    }

    #[test]
    fn catalog_snapshot_freezes_rows_and_revision_at_prepare() {
        let source = MutableCatalog {
            rows: Mutex::new(vec![row("model-a")]),
            revision: Mutex::new(7),
        };
        let snapshot = CatalogSnapshot::capture(&source).unwrap();
        let original = snapshot.revision();

        *source.rows.lock().unwrap() = vec![row("model-b")];
        *source.revision.lock().unwrap() = 8;

        assert_eq!(snapshot.revision(), original);
        assert!(snapshot.contains_route("profile", "model-a"));
        assert!(!snapshot.contains_route("profile", "model-b"));
    }

    #[test]
    fn price_snapshot_freezes_rates_for_the_prepared_routes() {
        let source = MutableCatalog {
            rows: Mutex::new(vec![row("model-a")]),
            revision: Mutex::new(1),
        };
        let catalog = CatalogSnapshot::capture(&source).unwrap();
        let prices = MutablePrices {
            rates: Mutex::new(Some(rates(3))),
        };
        let captured = catalog.capture_prices(&prices, &[]);
        *prices.rates.lock().unwrap() = Some(rates(99));

        assert_eq!(
            captured
                .rates_for("profile", "model-a")
                .unwrap()
                .input_nano_usd_per_token,
            3
        );
    }

    #[test]
    fn canonical_route_key_is_reused_without_relisting() {
        let source = MutableCatalog {
            rows: Mutex::new(vec![row("provider/model.v1")]),
            revision: Mutex::new(1),
        };
        let snapshot = CatalogSnapshot::capture(&source).unwrap();
        assert_eq!(
            snapshot.canonical_key_for("profile", "provider/model.v1"),
            "model-v1"
        );
    }

    #[test]
    fn route_limits_do_not_collapse_same_model_across_profiles() {
        let mut first = row("shared-model");
        first.profile = "profile-a".into();
        first.limits.max_input_tokens = Some(4_000);
        let mut second = first.clone();
        second.profile = "profile-b".into();
        second.limits.max_input_tokens = Some(8_000);
        let source = MutableCatalog {
            rows: Mutex::new(vec![first, second]),
            revision: Mutex::new(1),
        };
        let snapshot = CatalogSnapshot::capture(&source).unwrap();

        assert_eq!(
            snapshot
                .limits_for("profile-a", "shared-model")
                .unwrap()
                .max_input_tokens,
            Some(4_000)
        );
        assert_eq!(
            snapshot
                .limits_for("profile-b", "shared-model")
                .unwrap()
                .max_input_tokens,
            Some(8_000)
        );
    }

    struct ScriptedCatalog {
        rows: Mutex<std::collections::VecDeque<Vec<CatalogModel>>>,
        revisions: Mutex<std::collections::VecDeque<u64>>,
    }

    impl ModelSource for ScriptedCatalog {
        fn list(&self) -> Vec<CatalogModel> {
            let mut rows = self.rows.lock().unwrap();
            let value = rows.front().cloned().unwrap_or_default();
            if rows.len() > 1 {
                rows.pop_front();
            }
            value
        }

        fn revision(&self) -> u64 {
            let mut revisions = self.revisions.lock().unwrap();
            let value = revisions.front().copied().unwrap_or_default();
            if revisions.len() > 1 {
                revisions.pop_front();
            }
            value
        }
    }

    #[test]
    fn stable_capture_rejects_remove_readd_aba_even_when_rows_match() {
        let same_rows = vec![row("model-a")];
        let source = ScriptedCatalog {
            // The route is removed and re-added between the two collected
            // views, so content alone has returned to its original value.
            rows: Mutex::new(
                [
                    same_rows.clone(),
                    same_rows.clone(),
                    same_rows.clone(),
                    same_rows,
                ]
                .into(),
            ),
            revisions: Mutex::new([7, 9, 9, 9].into()),
        };

        assert!(CatalogSnapshot::try_capture_stable(&source).is_none());
        let stable = CatalogSnapshot::try_capture_stable(&source).expect("second view is stable");
        assert_eq!(stable.revision().source, 9);
        assert!(stable.contains_route("profile", "model-a"));
    }
}
