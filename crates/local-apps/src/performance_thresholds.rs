//! Phase-1 performance-baseline loader and validator.
//!
//! §19.0 of the Local App Plugin design requires a checked-in
//! `docs/local-apps/performance-baselines/local-app-plugin-v1.json` holding
//! ten non-zero hard limits, written *before* any measurement so the numbers
//! constrain the code instead of describing it. The contract is exact: "a
//! missing field, a value of 0, or a non-numeric placeholder must make
//! validation FAIL — not warn, not default." This module is that validator.
//!
//! ## Why `include_str!`, not a runtime read
//!
//! The baseline file lives outside every crate, at a repo-relative path
//! (`../../../docs/...` from this crate). Two ways to reach it were on the
//! table:
//!
//! - **Runtime read** (`std::fs::read_to_string` from a path derived at
//!   startup) would need the caller to hand in — or this crate to guess —
//!   the repo root, and a shipped binary carries no such anchor once it is
//!   copied off a checked-out worktree. Worse, the JSON could be hand-edited
//!   *after* a binary is built and validated, so "the tests passed" would
//!   stop meaning "the shipped binary enforces these limits."
//! - **`include_str!`** bakes the file's bytes into the binary at compile
//!   time. A later edit to the JSON cannot silently diverge from what a
//!   built artifact enforces — the only way to change the enforced
//!   thresholds is to edit the file and rebuild, which is exactly the
//!   "independent, evidenced design change" §19.0 requires anyway.
//!
//! This module takes the second option. [`load_baseline`] validates the
//! baked-in file; [`validate`] is the underlying parser exposed separately
//! so tests can feed it deliberately mutated fixtures without touching the
//! checked-in JSON.
//!
//! ## Structural coverage, not a hand-maintained field list
//!
//! Ten fields and a test that checks one of them proves nothing about the
//! other nine — and a hand-maintained "list of fields to check" drifts the
//! same way a hand-maintained field list anywhere else in this codebase
//! does. [`performance_thresholds!`] declares the field names exactly once;
//! that single list generates the struct, the per-field extraction, *and*
//! the unknown-field rejection below, so:
//!
//! - a field named in the macro but absent from the JSON fails as
//!   [`ThresholdError::MissingField`];
//! - a field present in the JSON but not named in the macro fails as
//!   [`ThresholdError::UnknownField`] instead of being silently ignored.
//!
//! A threshold added to the JSON without a matching check therefore cannot
//! pass silently — it has no field to be silent as. Extending coverage to a
//! new threshold means adding one identifier to the macro invocation, not
//! writing a new field-by-field check that could itself go stale.
//!
//! That symmetry has one blind spot, and the tests close it separately:
//! deleting a threshold from the macro list *and* the JSON at the same time
//! leaves nine enforced limits with nothing missing and nothing unknown. So
//! `tests::SECTION_19_0_FIELDS` transcribes the ten names from the design
//! doc by hand — an independent third list, on purpose — and
//! `the_enforced_field_set_is_exactly_the_ten_named_in_section_19_0` pins
//! both the macro's field list and the checked-in JSON's key set against it.

use serde_json::{Map, Value};
use thiserror::Error;

/// The Phase-1 performance baseline, checked in at
/// `docs/local-apps/performance-baselines/local-app-plugin-v1.json` and
/// baked into this binary at compile time (see the module docs for why).
const BASELINE_JSON: &str =
    include_str!("../../../docs/local-apps/performance-baselines/local-app-plugin-v1.json");

/// Every way a performance-baseline document can fail §19.0 validation.
///
/// Each variant names the offending field (or, for [`Self::UnknownField`],
/// the field with no matching check) so a caller — and a test — can assert
/// not just that validation failed, but *which* field and *why*.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ThresholdError {
    /// The document is not valid JSON at all.
    #[error("performance baseline is not valid JSON: {0}")]
    MalformedJson(String),
    /// The document parsed but is not a JSON object.
    #[error("performance baseline must be a JSON object")]
    NotAnObject,
    /// A required threshold field is absent.
    #[error("performance baseline is missing required field `{0}`")]
    MissingField(String),
    /// A required threshold field is present but its value is exactly zero.
    /// §19.0 forbids zero hard limits outright.
    #[error("performance baseline field `{0}` is 0; §19.0 forbids zero hard limits")]
    ZeroValue(String),
    /// A required threshold field holds a value that is not a positive
    /// integer — a string placeholder, `null`, a bool, a negative number, a
    /// non-integer float, an array, or an object.
    #[error(
        "performance baseline field `{field}` is not a positive integer (found {found}); \
         a non-numeric placeholder must fail validation, not default"
    )]
    NonNumericPlaceholder {
        /// The field whose value failed to parse as a positive integer.
        field: String,
        /// A short, human-readable description of what was found instead
        /// (e.g. `string "TBD"`, `bool true`, `null`).
        found: String,
    },
    /// The document contains a field this validator does not recognize —
    /// i.e. a threshold was added to the JSON with no matching check. This
    /// fails loudly rather than silently ignoring an unvalidated limit.
    #[error(
        "performance baseline has field `{0}` with no matching threshold check; \
         add it to the `performance_thresholds!` field list in this module"
    )]
    UnknownField(String),
}

/// Extract a required, strictly-positive `u64` named `field` from `obj`,
/// distinguishing every rejection §19.0 names.
fn extract_nonzero_u64(obj: &Map<String, Value>, field: &str) -> Result<u64, ThresholdError> {
    let value = obj
        .get(field)
        .ok_or_else(|| ThresholdError::MissingField(field.to_string()))?;
    match value {
        Value::Number(n) => match n.as_u64() {
            Some(0) => Err(ThresholdError::ZeroValue(field.to_string())),
            Some(v) => Ok(v),
            // A number serde could not read as a `u64`: negative, a float,
            // or out of `u64` range. `0.0` and `-0.0` are still *zero*, and
            // §19.0 forbids a zero hard limit by name — report the reason
            // the reader actually needs rather than the generic
            // "not a positive integer", which would be a rejection for the
            // wrong reason.
            None if n.as_f64() == Some(0.0) => Err(ThresholdError::ZeroValue(field.to_string())),
            None => Err(ThresholdError::NonNumericPlaceholder {
                field: field.to_string(),
                found: describe(value),
            }),
        },
        other => Err(ThresholdError::NonNumericPlaceholder {
            field: field.to_string(),
            found: describe(other),
        }),
    }
}

/// Short description of a JSON value's shape, for error messages.
fn describe(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool {b}"),
        Value::String(s) => format!("string {s:?}"),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
        // Only reached for a non-zero number that failed `as_u64` above:
        // negative, fractional, or out of `u64` range. Do not editorialise
        // about its sign here — the enclosing message already says what was
        // required, and `1e30` is positive yet still lands in this arm.
        Value::Number(n) => format!("number {n}"),
    }
}

/// Declares the field list for [`PerformanceThresholds`] exactly once,
/// generating the struct, [`PerformanceThresholds::FIELD_NAMES`], and the
/// per-field extraction + unknown-field rejection in
/// [`PerformanceThresholds::from_object`] from that single list. See the
/// module docs ("Structural coverage") for why this replaces a
/// hand-maintained field-by-field check list.
macro_rules! performance_thresholds {
    ($($field:ident),+ $(,)?) => {
        /// Ten non-zero hard limits from §19.0 of the Local App Plugin
        /// design, parsed and validated from
        /// `docs/local-apps/performance-baselines/local-app-plugin-v1.json`.
        /// Every field is a strictly-positive `u64` — this type cannot be
        /// constructed otherwise; see [`validate`] and [`load_baseline`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[allow(missing_docs)]
        pub struct PerformanceThresholds {
            $(pub $field: u64,)+
        }

        impl PerformanceThresholds {
            /// Every field this validator expects, in declaration order.
            /// The single source of truth for the struct's shape, the
            /// per-field extraction below, and unknown-field rejection.
            const FIELD_NAMES: &'static [&'static str] = &[$(stringify!($field)),+];

            fn from_object(obj: &Map<String, Value>) -> Result<Self, ThresholdError> {
                $(
                    let $field = extract_nonzero_u64(obj, stringify!($field))?;
                )+
                for key in obj.keys() {
                    if !Self::FIELD_NAMES.contains(&key.as_str()) {
                        return Err(ThresholdError::UnknownField(key.clone()));
                    }
                }
                Ok(Self { $($field),+ })
            }
        }
    };
}

performance_thresholds!(
    builtin_archive_max_bytes,
    builtin_extracted_max_bytes,
    first_materialization_p95_ms,
    cached_startup_added_p95_ms,
    published_apps_30_added_p95_ms,
    published_apps_100_added_p95_ms,
    logical_servers_100_registration_p95_ms,
    logical_servers_100_retained_heap_max_bytes,
    per_app_tool_definitions_max_tokens,
    expanded_tool_definitions_max_tokens,
);

/// Parse and validate a performance-baseline JSON document.
///
/// Fails on a document that is not valid JSON, is not a JSON object, is
/// missing any of the ten required fields, has any field equal to `0`, has
/// any field holding a non-numeric placeholder (or any other non-positive
/// integer value), or carries a field this validator does not recognize.
pub fn validate(json: &str) -> Result<PerformanceThresholds, ThresholdError> {
    let value: Value =
        serde_json::from_str(json).map_err(|e| ThresholdError::MalformedJson(e.to_string()))?;
    let obj = value.as_object().ok_or(ThresholdError::NotAnObject)?;
    PerformanceThresholds::from_object(obj)
}

/// Load and validate the checked-in Phase-1 performance baseline, baked
/// into this binary at compile time via `include_str!` (see the module
/// docs). Returns the same [`ThresholdError`] as [`validate`] on failure —
/// a broken checked-in baseline fails the build's test suite, not silently
/// falls back to a default.
pub fn load_baseline() -> Result<PerformanceThresholds, ThresholdError> {
    validate(BASELINE_JSON)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The checked-in baseline, reparsed as a mutable map so tests can
    /// mutate exactly one field away from a document that is otherwise
    /// known-valid — never a synthetic fixture built from scratch, which
    /// could silently omit the very field under test.
    fn baseline_map() -> Map<String, Value> {
        serde_json::from_str::<Value>(BASELINE_JSON)
            .expect("checked-in baseline must be valid JSON")
            .as_object()
            .expect("checked-in baseline must be a JSON object")
            .clone()
    }

    /// The ten field names §19.0 of `LOCAL-APP-PLUGIN-DESIGN-V2.md` lists
    /// verbatim, transcribed from the spec by hand.
    ///
    /// Deliberately NOT derived from [`PerformanceThresholds::FIELD_NAMES`]
    /// nor from the checked-in JSON. A set derived from the thing it checks
    /// is vacuous: it would stay green through the one edit this gate exists
    /// to stop — dropping a threshold from the macro list *and* the JSON
    /// together (the shape a "make the build green" pass produces), which
    /// leaves nine enforced limits, no unknown field, no missing field, and
    /// nothing red.
    const SECTION_19_0_FIELDS: [&str; 10] = [
        "builtin_archive_max_bytes",
        "builtin_extracted_max_bytes",
        "first_materialization_p95_ms",
        "cached_startup_added_p95_ms",
        "published_apps_30_added_p95_ms",
        "published_apps_100_added_p95_ms",
        "logical_servers_100_registration_p95_ms",
        "logical_servers_100_retained_heap_max_bytes",
        "per_app_tool_definitions_max_tokens",
        "expanded_tool_definitions_max_tokens",
    ];

    /// Pins *which* thresholds exist, on both sides of the seam, against the
    /// spec — the structural half that per-field value assertions cannot
    /// cover, because a field deleted from every list is a field no
    /// value assertion mentions any more.
    #[test]
    fn the_enforced_field_set_is_exactly_the_ten_named_in_section_19_0() {
        let mut spec = SECTION_19_0_FIELDS.to_vec();
        spec.sort_unstable();

        // `FIELD_NAMES` is macro-generated, so this count is a real
        // measurement of the validator, not a restatement of the literal
        // above. It names both numbers when it fails.
        assert_eq!(
            PerformanceThresholds::FIELD_NAMES.len(),
            SECTION_19_0_FIELDS.len(),
            "the validator enforces {} thresholds; §19.0 names {}",
            PerformanceThresholds::FIELD_NAMES.len(),
            SECTION_19_0_FIELDS.len(),
        );

        let mut declared = PerformanceThresholds::FIELD_NAMES.to_vec();
        declared.sort_unstable();
        assert_eq!(
            declared, spec,
            "the `performance_thresholds!` field list has drifted from §19.0 of \
             docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2.md",
        );

        let mut on_disk: Vec<String> = baseline_map().keys().cloned().collect();
        on_disk.sort();
        assert_eq!(
            on_disk, spec,
            "the key set of \
             docs/local-apps/performance-baselines/local-app-plugin-v1.json \
             has drifted from §19.0",
        );
    }

    #[test]
    fn checked_in_baseline_loads_and_matches_the_design_doc() {
        // Positive control for every rejection test below: confirms the
        // unmutated fixture validates and every one of the ten §19.0 values
        // round-trips exactly, so a rejection test's failure can only be
        // caused by the one mutation it makes, not a validator that never
        // accepts anything.
        let t = load_baseline().expect("checked-in baseline must validate");

        // Field-name/value pairs rather than ten bare `assert_eq!`s: a bare
        // one reports only `left: 16385 / right: 16384` and a line number,
        // leaving the reader to look up which limit drifted. Comparing the
        // labelled lists prints both in full, so the failure names the
        // threshold.
        let got = vec![
            ("builtin_archive_max_bytes", t.builtin_archive_max_bytes),
            ("builtin_extracted_max_bytes", t.builtin_extracted_max_bytes),
            (
                "first_materialization_p95_ms",
                t.first_materialization_p95_ms,
            ),
            ("cached_startup_added_p95_ms", t.cached_startup_added_p95_ms),
            (
                "published_apps_30_added_p95_ms",
                t.published_apps_30_added_p95_ms,
            ),
            (
                "published_apps_100_added_p95_ms",
                t.published_apps_100_added_p95_ms,
            ),
            (
                "logical_servers_100_registration_p95_ms",
                t.logical_servers_100_registration_p95_ms,
            ),
            (
                "logical_servers_100_retained_heap_max_bytes",
                t.logical_servers_100_retained_heap_max_bytes,
            ),
            (
                "per_app_tool_definitions_max_tokens",
                t.per_app_tool_definitions_max_tokens,
            ),
            (
                "expanded_tool_definitions_max_tokens",
                t.expanded_tool_definitions_max_tokens,
            ),
        ];

        // Structural: an eleventh threshold added to the macro, the JSON and
        // §19.0 would sail past a value check that simply forgot to mention
        // it. This makes "a threshold with no pinned value" red.
        assert_eq!(
            got.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
            SECTION_19_0_FIELDS.to_vec(),
            "this test pins values for {} threshold(s); §19.0 names {}",
            got.len(),
            SECTION_19_0_FIELDS.len(),
        );

        assert_eq!(
            got,
            vec![
                ("builtin_archive_max_bytes", 4_194_304u64),
                ("builtin_extracted_max_bytes", 12_582_912),
                ("first_materialization_p95_ms", 2_000),
                ("cached_startup_added_p95_ms", 150),
                ("published_apps_30_added_p95_ms", 75),
                ("published_apps_100_added_p95_ms", 200),
                ("logical_servers_100_registration_p95_ms", 250),
                ("logical_servers_100_retained_heap_max_bytes", 16_777_216),
                ("per_app_tool_definitions_max_tokens", 2_048),
                ("expanded_tool_definitions_max_tokens", 16_384),
            ],
            "a checked-in threshold has drifted from §19.0 of \
             docs/local-apps/LOCAL-APP-PLUGIN-DESIGN-V2.md",
        );
    }

    #[test]
    fn a_zero_threshold_is_rejected() {
        let mut obj = baseline_map();
        obj.insert("cached_startup_added_p95_ms".to_string(), json!(0));
        let err = validate(&Value::Object(obj).to_string())
            .expect_err("a zero hard limit must be rejected, not accepted");
        assert_eq!(
            err,
            ThresholdError::ZeroValue("cached_startup_added_p95_ms".to_string()),
            "must name the zeroed field specifically, not just fail generically"
        );
    }

    #[test]
    fn a_missing_field_is_rejected() {
        let mut obj = baseline_map();
        let removed = obj.remove("expanded_tool_definitions_max_tokens");
        assert!(
            removed.is_some(),
            "test fixture bug: field was already absent"
        );
        let err = validate(&Value::Object(obj).to_string())
            .expect_err("a missing required field must be rejected, not defaulted");
        assert_eq!(
            err,
            ThresholdError::MissingField("expanded_tool_definitions_max_tokens".to_string()),
            "must name the missing field specifically, not just fail generically"
        );
    }

    #[test]
    fn a_non_numeric_placeholder_is_rejected() {
        let mut obj = baseline_map();
        obj.insert(
            "per_app_tool_definitions_max_tokens".to_string(),
            json!("TBD"),
        );
        let err = validate(&Value::Object(obj).to_string())
            .expect_err("a non-numeric placeholder must be rejected, not coerced to a default");
        assert_eq!(
            err,
            ThresholdError::NonNumericPlaceholder {
                field: "per_app_tool_definitions_max_tokens".to_string(),
                found: "string \"TBD\"".to_string(),
            },
            "must name the offending field and what was found in place of a number"
        );
    }

    /// Positive control against the axis-correlation failure mode this
    /// batch calls out by name: zero out each of the ten fields ONE AT A
    /// TIME (not a single hard-coded field) and require the error to name
    /// that exact field. A validator that only checks, say, the first field
    /// it happens to look at — or a length/count proxy that stays green
    /// regardless of which field changed — would fail this loop even though
    /// `a_zero_threshold_is_rejected` above could still pass.
    #[test]
    fn every_field_independently_rejects_zero() {
        let mut checked = Vec::new();
        for field in PerformanceThresholds::FIELD_NAMES {
            let mut obj = baseline_map();
            obj.insert((*field).to_string(), json!(0));
            let err = validate(&Value::Object(obj).to_string()).expect_err(&format!(
                "field `{field}` was zeroed but validate() returned Ok"
            ));
            assert_eq!(
                err,
                ThresholdError::ZeroValue((*field).to_string()),
                "zeroing `{field}` must name `{field}` in the error, not some other field"
            );
            checked.push(*field);
        }
        // In-test positive control: without it, a green run would be
        // consistent with the loop body never executing at all.
        assert_eq!(
            checked.len(),
            SECTION_19_0_FIELDS.len(),
            "the zero probe fired for {} field(s) ({checked:?}); §19.0 has {}",
            checked.len(),
            SECTION_19_0_FIELDS.len(),
        );
    }

    /// Guards the structural-coverage requirement directly: a threshold
    /// added to the JSON with no matching field in
    /// `PerformanceThresholds` must fail, not pass silently because nothing
    /// looked at it.
    #[test]
    fn an_unmatched_field_added_to_the_json_is_rejected() {
        let mut obj = baseline_map();
        obj.insert("worker_pool_p95_ms".to_string(), json!(500));
        let err = validate(&Value::Object(obj).to_string())
            .expect_err("a field with no matching check must be rejected, not ignored");
        assert_eq!(
            err,
            ThresholdError::UnknownField("worker_pool_p95_ms".to_string()),
            "must name the unmatched field specifically"
        );
    }

    #[test]
    fn a_negative_number_is_rejected_as_non_numeric() {
        // Not one of the three required tests, but the same code path: a
        // JSON number that cannot be a `u64` (negative here) must fail the
        // same way a string placeholder does, not silently truncate/cast.
        let mut obj = baseline_map();
        obj.insert("builtin_archive_max_bytes".to_string(), json!(-1));
        let err = validate(&Value::Object(obj).to_string())
            .expect_err("a negative value must be rejected, not cast to an unsigned value");
        assert_eq!(
            err,
            ThresholdError::NonNumericPlaceholder {
                field: "builtin_archive_max_bytes".to_string(),
                found: "number -1".to_string(),
            }
        );
    }

    #[test]
    fn a_zero_written_as_a_float_is_rejected_as_a_zero_value_not_as_a_placeholder() {
        // "Rejected" is not enough: a rejection for the wrong reason passes
        // the weak `is_err()` form of this test. `0.0` is not a `u64`, so the
        // naive reading is "non-numeric placeholder" — but §19.0 forbids a
        // *zero* hard limit specifically, and that is the reason the error
        // must carry.
        for zero in [json!(0.0), json!(-0.0)] {
            let mut obj = baseline_map();
            obj.insert("first_materialization_p95_ms".to_string(), zero.clone());
            let err = validate(&Value::Object(obj).to_string())
                .expect_err("a zero hard limit must be rejected however it is spelled");
            assert_eq!(
                err,
                ThresholdError::ZeroValue("first_materialization_p95_ms".to_string()),
                "`{zero}` is a zero hard limit; the error must say so, not blame a placeholder"
            );
        }
    }

    #[test]
    fn malformed_json_is_rejected() {
        let err = validate("not json").expect_err("malformed JSON must be rejected, not defaulted");
        assert!(matches!(err, ThresholdError::MalformedJson(_)));
    }

    #[test]
    fn a_non_object_document_is_rejected() {
        let err = validate("[1,2,3]").expect_err("a non-object document must be rejected");
        assert_eq!(err, ThresholdError::NotAnObject);
    }
}
