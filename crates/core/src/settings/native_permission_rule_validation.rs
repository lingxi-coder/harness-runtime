//! Acceptance-only draft for Native managed `permissions` validation.
//!
//! This file is deliberately outside the production crate. It is a source-bound
//! port candidate for `crates/core/src/settings/native_permission_rule_validation.rs`;
//! it was not compiled or integrated in this task.
//!
//! Fixed-source bindings (Claude Code 2.1.295):
//! - The import map is the retained `native-bundle-manifest.json` whose SHA-256
//!   is `f57e71e53deb242e73a8114c3d86359f035bed026918a1e60dcb4d1143d21e28`;
//!   the raw ranges below were separately rehashed from the binary SHA-256
//!   `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
//! - `src_187673145.js`, byte 187673145, 434959 bytes, SHA-256
//!   `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`.
//! - It imports `Wue`, `$n`, `mo`, and `bn` from chunk-cwvf0n7t, bound by
//!   `native-bundle-manifest.json` to `src_186325155.js`, byte 186325155,
//!   7419 bytes, SHA-256
//!   `a66f6f6d6f121ab2a8a9b5acf8ad8fa5d95ce3c6b55175dd7bbf5463b3567f0b`.
//! - It imports `oh` and `fH` from chunk-cbvmkarm, bound to
//!   `src_186804723.js`, byte 186804723, 7762 bytes, SHA-256
//!   `2e805f6db99e074a6f2ec8c15c75897b49d722419230b5b34afcdfe5498b5fbe`.
//! - It imports `Sa` from chunk-dntn27yg, bound to `src_186231195.js`,
//!   byte 186231195, 7300 bytes, SHA-256
//!   `125f57566b99b67ce958d77cd787163124654cbbb3bdd6f3a92dfd38955a2317`.
//! - Wue is defined at `src_186325155.js` byte 186327901; `$l`, `DCo`, and
//!   `$n` are at 186326098, 186327778, and 186328217. The imported `oh` and
//!   `fH` are at 186805442 and 186805338; imported `Sa` is at 186231934.
//!
//! Relevant declarations/callers in `src_187673145.js`:
//! - `y0t` 187859020; `fxe` 187859439..187863882; `ta` 187863882;
//!   `ma` 187864234; permissions field factory 187892556.
//! - `d_` 188046507 filters `permissions.allow/deny/ask` in that order and
//!   calls `fxe`; it emits one warning per dropped array member. `es` at
//!   187955214 applies the permission allow-floor after all three arrays.
//! - `At` 188060543 calls `d_(..., { policySource, strictParseFollows })`;
//!   `fTr` 188044910 calls `At(..., { mcpServerEntrySalvageOnly: true,
//!   policySource: true })`, so that path enables strictParseFollows.
//! - `ma` at 187864234 takes `e` from `Hdt()`; `defaultMode` at byte
//!   187864542 is `Yi(oh, U([...fH, ...zo(e)]))`. `Hdt` at 187853882 filters
//!   `jg` against each feature's `buildGate`; `jg` at 187851443 is
//!   `autoMode, deepLink, voice, briefView, screenReader`, and each gate in
//!   this fixed build is `()=>true`. `zo` at 187854099 flattens each enabled
//!   feature's optional `permissionModes`. The only such provider is
//!   `autoMode` at 187853083, which returns `HE.filter(e => !fH.includes(e))`.
//!   In the bound `src_186804723.js`, `fH` is exactly
//!   `acceptEdits, auto, bypassPermissions, default, dontAsk, plan`, and
//!   `HE=[...fH]`; therefore the feature-derived extension is empty and the
//!   exact final `defaultMode` enum is that six-item list in the same order.
//!   No provider, host, or runtime fact extends it. `oh` maps `manual` to
//!   `default`. The allow-floor's `En` entry uses the same fixed mode list,
//!   treats `default`, `dontAsk`, and `plan` as inert, and substitutes
//!   `default` for a non-inert mode.
//!
//! - Imported `Sa` is defined in `src_186231195.js` at byte 186231934 as
//!   `charAt(0).toUpperCase()+slice(1)`. `fxe` compares only
//!   `toolName[0]` with `toolName[0]?.toUpperCase()` (187860733); it calls
//!   `Sa(String(toolName))` only to build the lowercase-first suggestion
//!   (187860858). It does not uppercase the whole tool name. Unicode
//!   case conversion is generated for exactly that first UTF-16 code unit;
//!   non-ASCII suffixes/content do not enter the case check.
//! - The permission mode enum is `U([...])` from `src_186578912.js` at
//!   186665555; `Yi` constructs the input pipe at 186669135. Its Zod enum
//!   parser records an `invalid_value` issue with the complete six-value list.
//!   The source locale formatter emits `Invalid option: expected one of `
//!   followed by `qCo(value)` strings joined with `|`; `qCo` quotes strings.
//!   `fTr` at 188044910 takes the first three Due issues and formats this
//!   path as `permissions.defaultMode: <message>`; `y_` at 188052577 leaves
//!   it unchanged because its second component is a string, not an array index.
//!   The exact issue is retained on `NativePermissionDefaultModeResult` for
//!   the static-settings adapter.
//!
//! `fxe` itself has no platform argument. The default-mode list is fully
//! closed by the fixed `fH` constant and empty `zo(Hdt())` extension below.
//! ASCII regexes in `fxe` are implemented with UTF-16-unit scanners. For a
//! non-ASCII first unit the case mapping is table-driven from the isolated
//! Bun 1.4.3 / Unicode 16.0 pure-JS probe recorded at
//! `acceptance/source-alignment-295-r31/bun-1.4.3-oracle-runtime.json`; it
//! enumerated `String.fromCharCode(unit).toUpperCase()` for all 65,536 code
//! units, including multi-unit expansions. The table generation did not run
//! the Native CLI.

use crate::types::utf16_json::{JsonValueView, Utf16JsonBuilder, Utf16JsonProjection};

const PERMISSION_BUCKETS: [&str; 3] = ["allow", "deny", "ask"];
const PERMISSION_RESTRICTIONS: [&str; 2] = ["deny", "ask"];
const PERMISSION_GRANTS: [&str; 3] = ["allow", "additionalDirectories", "defaultMode"];
const INERT_DEFAULT_MODES: [&str; 3] = ["default", "dontAsk", "plan"];
pub(crate) const NATIVE_PERMISSION_MODES: [&str; 6] = [
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePermissionRuleClass {
    Allow,
    Deny,
    Ask,
}

impl NativePermissionRuleClass {
    const fn is_allow(self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionSourceContext {
    /// Native's `At(e, n, options)` diagnostic file argument.
    pub file: String,
    /// Exact source option `options.policySource`.
    pub policy_source: bool,
    /// Exact source option `options.mcpServerEntrySalvageOnly`, which `At`
    /// forwards to `d_` as `strictParseFollows`.
    pub strict_parse_follows: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePermissionRuleStatus {
    Valid,
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePermissionSyntaxKind {
    Bare,
    Call,
    Malformed,
    NotParsed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionRuleResult {
    /// Original JSON string projection; never reconstructed from a lossy Rust
    /// `String`.
    pub original: Utf16JsonProjection,
    pub class: NativePermissionRuleClass,
    pub syntax: NativePermissionSyntaxKind,
    /// Raw Wue `toolName` and `rawContent` slices, measured in UTF-16 units.
    pub tool_name: Option<Vec<u16>>,
    pub raw_content: Option<Vec<u16>>,
    /// `$n`'s aliased tool name and DCo-normalized content.
    pub normalized_tool_name: Option<Vec<u16>>,
    pub normalized_content: Option<Vec<u16>>,
    pub status: NativePermissionRuleStatus,
    /// Whether d_ keeps/drops this member when its bucket is fully classified.
    /// `None` means the bucket result is unresolved and the original array is
    /// retained untouched by this draft.
    pub kept: Option<bool>,
    /// Native `fxe` output values, serialized as exact UTF-16 string values.
    pub error: Option<Utf16JsonProjection>,
    pub suggestion: Option<Utf16JsonProjection>,
    pub examples: Vec<Utf16JsonProjection>,
    /// fxe advisory warning. Native `d_` currently does not surface this
    /// warning when the rule is valid; it is retained here for callers that
    /// need the complete validator result.
    pub warning: Option<Utf16JsonProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePermissionArrayStatus {
    Absent,
    PresentNonArray,
    Readable,
    Trimmed,
    Unreadable,
    /// The fixed source has an explicit preserve-without-filter branch when
    /// policySource is true and strictParseFollows is false.
    PreservedWithoutFiltering,
    Unresolved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionBucketResult {
    pub field: &'static str,
    pub status: NativePermissionArrayStatus,
    /// Members in original source order. Non-string JSON members have
    /// `syntax=NotParsed`, `status=Invalid`, and no fxe error.
    pub entries: Vec<NativePermissionRuleResult>,
    /// Effective array contents when this draft can classify the bucket.
    /// For Unresolved, this is a conservative copy of the input array and the
    /// caller must reject/mark the containing field unresolved.
    pub kept_rules: Vec<Utf16JsonProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePermissionDiagnosticKind {
    NonStringEntryRemoved,
    InvalidRuleSkipped,
    GrantWithheld,
    DefaultModeFloored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionDiagnostic {
    pub file: String,
    pub path: String,
    pub kind: NativePermissionDiagnosticKind,
    /// `None` only when a restriction bucket is unresolved, so the exact
    /// aggregate allow-floor message cannot be reproduced.
    pub message: Option<Utf16JsonProjection>,
    pub invalid_value: Option<Utf16JsonProjection>,
    pub substituted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionWithheld {
    pub field: &'static str,
    pub original: Utf16JsonProjection,
    pub floor: Option<Utf16JsonProjection>,
    pub message: Option<Utf16JsonProjection>,
    pub substituted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativePermissionDefaultModeStatus {
    Absent,
    Valid,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionDueIssue {
    pub path: &'static str,
    pub message: Utf16JsonProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionDefaultModeResult {
    pub status: NativePermissionDefaultModeStatus,
    pub original: Option<Utf16JsonProjection>,
    /// `manual` is returned as `default`, matching imported `oh`.
    pub normalized: Option<Utf16JsonProjection>,
    /// Exact Zod issue consumed by `fTr` when Due rejects this field.
    pub due_issue: Option<NativePermissionDueIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePermissionResult {
    /// Source-ordered projection after d_ filtering, es allow-floor, and the
    /// defaultMode `oh` pipe transform where the enum accepts the value.
    pub normalized: Utf16JsonProjection,
    pub buckets: Vec<NativePermissionBucketResult>,
    /// d_ member warnings followed by es grant-withheld warnings.
    pub diagnostics: Vec<NativePermissionDiagnostic>,
    pub withheld: Vec<NativePermissionWithheld>,
    pub default_mode: NativePermissionDefaultModeResult,
    /// Any unresolved permission bucket prevents callers from treating
    /// `normalized` as a complete Native-equivalent policy value.
    pub unresolved_fields: Vec<String>,
}

impl NativePermissionResult {
    /// Whether the three permission rule arrays are in a state that this
    /// source-bound draft can hand to the strict `ta`/`Due` array validator.
    /// This says nothing about `defaultMode`, `additionalDirectories`, other
    /// passthrough properties, or the containing `permissions` object's full
    /// schema validity.
    #[must_use]
    pub(crate) fn rule_arrays_due_valid(&self) -> bool {
        self.buckets.len() == PERMISSION_BUCKETS.len()
            && PERMISSION_BUCKETS.iter().all(|field| {
                self.buckets
                    .iter()
                    .find(|bucket| bucket.field == *field)
                    .is_some_and(|bucket| {
                        matches!(
                            bucket.status,
                            NativePermissionArrayStatus::Absent
                                | NativePermissionArrayStatus::Readable
                                | NativePermissionArrayStatus::Trimmed
                                | NativePermissionArrayStatus::Unreadable
                        )
                    })
            })
    }
}

/// Normalize one `permissions` object with Native's `d_`/`es` behavior.
///
/// `fxe` rejects invalid strings; d_ filters invalid and non-string array
/// members in allow/deny/ask order, then invokes es. For permissions, only an
/// `unreadable` deny/ask bucket activates the floor; partial `trimmed` buckets
/// do not because the permissions map has no `withholdOnEntryDrop` flag.
#[must_use]
pub(crate) fn normalize_native_permissions(
    permissions: &Utf16JsonProjection,
    context: &NativePermissionSourceContext,
) -> NativePermissionResult {
    let Some(root) = permissions.at_pointer("") else {
        return unresolved_root_result(permissions.clone());
    };
    if root.object_entries().is_none() {
        return NativePermissionResult {
            normalized: permissions.clone(),
            buckets: PERMISSION_BUCKETS
                .iter()
                .map(|field| NativePermissionBucketResult {
                    field,
                    status: NativePermissionArrayStatus::PresentNonArray,
                    entries: Vec::new(),
                    kept_rules: Vec::new(),
                })
                .collect(),
            diagnostics: Vec::new(),
            withheld: Vec::new(),
            default_mode: NativePermissionDefaultModeResult {
                status: NativePermissionDefaultModeStatus::Absent,
                original: None,
                normalized: None,
                due_issue: None,
            },
            unresolved_fields: vec!["permissions".to_owned()],
        };
    }

    let mut buckets = Vec::with_capacity(PERMISSION_BUCKETS.len());
    let mut diagnostics = Vec::new();
    let mut unresolved_fields = Vec::new();
    let mut replacements = Vec::<(Vec<u16>, Utf16JsonProjection)>::new();
    let mut removals = Vec::<Vec<u16>>::new();

    for field in PERMISSION_BUCKETS {
        let Some(array_view) = root.get_property(field) else {
            buckets.push(NativePermissionBucketResult {
                field,
                status: NativePermissionArrayStatus::Absent,
                entries: Vec::new(),
                kept_rules: Vec::new(),
            });
            continue;
        };
        let Some(length) = array_view.array_len() else {
            buckets.push(NativePermissionBucketResult {
                field,
                status: NativePermissionArrayStatus::PresentNonArray,
                entries: Vec::new(),
                kept_rules: Vec::new(),
            });
            continue;
        };

        let rule_class = match field {
            "allow" => NativePermissionRuleClass::Allow,
            "deny" => NativePermissionRuleClass::Deny,
            "ask" => NativePermissionRuleClass::Ask,
            _ => unreachable!("the bucket list is fixed above"),
        };
        let mut entries = Vec::with_capacity(length);
        let mut source_order_kept = Vec::with_capacity(length);
        let mut invalid_seen = false;
        let mut unresolved_seen = false;
        let mut known_valid_count = 0usize;

        for index in 0..length {
            let Some(item_view) = array_view.get_index(index) else {
                unresolved_seen = true;
                continue;
            };
            let original = projection_from_view(item_view);
            let Some(units) = item_view.code_units() else {
                invalid_seen = true;
                diagnostics.push(NativePermissionDiagnostic {
                    file: context.file.clone(),
                    path: format!("permissions.{field}"),
                    kind: NativePermissionDiagnosticKind::NonStringEntryRemoved,
                    message: Some(string_projection(ascii_units(match field {
                        "allow" => "Non-string value in allow array was removed",
                        "deny" => "Non-string value in deny array was removed",
                        "ask" => "Non-string value in ask array was removed",
                        _ => unreachable!(),
                    }))),
                    invalid_value: Some(original.clone()),
                    substituted: false,
                });
                entries.push(NativePermissionRuleResult {
                    original,
                    class: rule_class,
                    syntax: NativePermissionSyntaxKind::NotParsed,
                    tool_name: None,
                    raw_content: None,
                    normalized_tool_name: None,
                    normalized_content: None,
                    status: NativePermissionRuleStatus::Invalid,
                    kept: Some(false),
                    error: None,
                    suggestion: None,
                    examples: Vec::new(),
                    warning: None,
                });
                continue;
            };

            let mut result = validate_native_permission_rule(units, rule_class, original);
            match result.status {
                NativePermissionRuleStatus::Valid => {
                    known_valid_count += 1;
                    source_order_kept.push(result.original.clone());
                    result.kept = Some(true);
                }
                NativePermissionRuleStatus::Invalid => {
                    invalid_seen = true;
                    let error_units = result
                        .error
                        .as_ref()
                        .and_then(|value| value.string_units(""))
                        .unwrap_or_default();
                    let mut message = compose_units(&[
                        TextPart::Static("Invalid permission rule \""),
                        TextPart::Units(units),
                        TextPart::Static("\" was skipped: "),
                        TextPart::Units(&error_units),
                    ]);
                    if let Some(suggestion) = result
                        .suggestion
                        .as_ref()
                        .and_then(|value| value.string_units(""))
                    {
                        message.extend(ascii_units(". "));
                        message.extend(suggestion);
                    }
                    diagnostics.push(NativePermissionDiagnostic {
                        file: context.file.clone(),
                        path: format!("permissions.{field}"),
                        kind: NativePermissionDiagnosticKind::InvalidRuleSkipped,
                        message: Some(string_projection(message)),
                        invalid_value: Some(result.original.clone()),
                        substituted: false,
                    });
                    result.kept = Some(false);
                }
            }
            entries.push(result);
        }

        let policy_empty_non_allow =
            context.policy_source && field != "allow" && length > 0 && known_valid_count == 0;
        let status = if unresolved_seen {
            NativePermissionArrayStatus::Unresolved
        } else if policy_empty_non_allow && context.strict_parse_follows {
            NativePermissionArrayStatus::Unreadable
        } else if policy_empty_non_allow {
            // The source continues before assigning the filtered array, so
            // invalid members remain in this compatibility branch.
            for entry in &mut entries {
                entry.kept = Some(true);
            }
            source_order_kept = entries.iter().map(|entry| entry.original.clone()).collect();
            NativePermissionArrayStatus::PreservedWithoutFiltering
        } else if invalid_seen {
            NativePermissionArrayStatus::Trimmed
        } else {
            source_order_kept = entries.iter().map(|entry| entry.original.clone()).collect();
            NativePermissionArrayStatus::Readable
        };

        let effective_kept = match status {
            NativePermissionArrayStatus::Unreadable => Vec::new(),
            NativePermissionArrayStatus::Unresolved
            | NativePermissionArrayStatus::PreservedWithoutFiltering => {
                entries.iter().map(|entry| entry.original.clone()).collect()
            }
            _ => source_order_kept,
        };

        if matches!(
            status,
            NativePermissionArrayStatus::Unreadable
                | NativePermissionArrayStatus::Trimmed
                | NativePermissionArrayStatus::Readable
        ) {
            replacements.push((
                ascii_units(field),
                Utf16JsonProjection::array(effective_kept.clone())
                    .expect("Native permission array members remain valid projections"),
            ));
        }

        buckets.push(NativePermissionBucketResult {
            field,
            status,
            entries,
            kept_rules: effective_kept,
        });
    }

    let known_unreadable = PERMISSION_RESTRICTIONS.iter().any(|field| {
        buckets
            .iter()
            .find(|bucket| bucket.field == *field)
            .is_some_and(|bucket| bucket.status == NativePermissionArrayStatus::Unreadable)
    });
    let uncertain_restriction = PERMISSION_RESTRICTIONS.iter().any(|field| {
        buckets
            .iter()
            .find(|bucket| bucket.field == *field)
            .is_some_and(|bucket| bucket.status == NativePermissionArrayStatus::Unresolved)
    });
    let restriction_reasons = PERMISSION_RESTRICTIONS
        .iter()
        .filter(|field| {
            buckets
                .iter()
                .find(|bucket| bucket.field == **field)
                .is_some_and(|bucket| bucket.status == NativePermissionArrayStatus::Unreadable)
        })
        .map(|field| format!("\"{field}\""))
        .collect::<Vec<_>>();

    let mut withheld = Vec::new();
    let mut default_mode = NativePermissionDefaultModeResult {
        status: NativePermissionDefaultModeStatus::Absent,
        original: None,
        normalized: None,
        due_issue: None,
    };
    if known_unreadable {
        for grant in PERMISSION_GRANTS {
            let Some(grant_view) = root.get_property(grant) else {
                continue;
            };
            let original = projection_from_view(grant_view);
            if grant == "defaultMode" {
                let parsed = normalize_default_mode(&original);
                default_mode = parsed.clone();
                let Some(normalized) = parsed.normalized.as_ref() else {
                    continue;
                };
                let Some(mode_units) = normalized.string_units("") else {
                    continue;
                };
                if INERT_DEFAULT_MODES
                    .iter()
                    .any(|inert| eq_ascii(&mode_units, inert))
                {
                    continue;
                }
                let floor = string_projection(ascii_units("default"));
                let message = (!uncertain_restriction).then(|| {
                    string_projection(compose_units(&[
                        TextPart::Static("\"defaultMode\" was withheld because "),
                        TextPart::Units(&compose_reasons(&restriction_reasons)),
                        TextPart::Static(
                            " in the same block could not be read; treating it as \"default\" until that is fixed.",
                        ),
                    ]))
                });
                replacements.retain(|(key, _)| !eq_ascii(key, "defaultMode"));
                replacements.push((ascii_units("defaultMode"), floor.clone()));
                default_mode.normalized = Some(floor.clone());
                withheld.push(NativePermissionWithheld {
                    field: "defaultMode",
                    original,
                    floor: Some(floor),
                    message: message.clone(),
                    substituted: true,
                });
                diagnostics.push(NativePermissionDiagnostic {
                    file: context.file.clone(),
                    path: "permissions.defaultMode".to_owned(),
                    kind: NativePermissionDiagnosticKind::DefaultModeFloored,
                    message,
                    invalid_value: None,
                    substituted: true,
                });
            } else {
                let message = (!uncertain_restriction).then(|| {
                    string_projection(compose_units(&[
                        TextPart::Static("\""),
                        TextPart::Static(grant),
                        TextPart::Static("\" was withheld because "),
                        TextPart::Units(&compose_reasons(&restriction_reasons)),
                        TextPart::Static(
                            " in the same block could not be read; it takes effect again once that is fixed.",
                        ),
                    ]))
                });
                removals.push(ascii_units(grant));
                replacements.retain(|(key, _)| !eq_ascii(key, grant));
                withheld.push(NativePermissionWithheld {
                    field: grant,
                    original,
                    floor: None,
                    message: message.clone(),
                    substituted: false,
                });
                diagnostics.push(NativePermissionDiagnostic {
                    file: context.file.clone(),
                    path: format!("permissions.{grant}"),
                    kind: NativePermissionDiagnosticKind::GrantWithheld,
                    message,
                    invalid_value: None,
                    substituted: false,
                });
            }
        }
    }

    if uncertain_restriction
        && !known_unreadable
        && !unresolved_fields.iter().any(|path| path == "permissions")
    {
        unresolved_fields.push("permissions".to_owned());
    }

    if default_mode.status == NativePermissionDefaultModeStatus::Absent {
        if let Some(mode_view) = root.get_property("defaultMode") {
            let original = projection_from_view(mode_view);
            default_mode = normalize_default_mode(&original);
        }
    }
    let default_mode_was_floored = withheld
        .iter()
        .any(|entry| entry.field == "defaultMode" && entry.substituted);
    if !default_mode_was_floored {
        if let Some(normalized) = default_mode.normalized.clone() {
            replacements.retain(|(key, _)| !eq_ascii(key, "defaultMode"));
            replacements.push((ascii_units("defaultMode"), normalized));
        }
    }

    let normalized = apply_object_changes(permissions, &replacements, &removals);
    NativePermissionResult {
        normalized,
        buckets,
        diagnostics,
        withheld,
        default_mode,
        unresolved_fields,
    }
}

/// Source `oh` followed by the enum input to `ma(e)`. A missing field is
/// represented by the caller; this function accepts one present projection.
#[must_use]
pub(crate) fn normalize_default_mode(
    value: &Utf16JsonProjection,
) -> NativePermissionDefaultModeResult {
    let original = value.clone();
    let Some(units) = value.at_pointer("").and_then(JsonValueView::code_units) else {
        return NativePermissionDefaultModeResult {
            status: NativePermissionDefaultModeStatus::Invalid,
            original: Some(original),
            normalized: None,
            due_issue: Some(native_default_mode_due_issue()),
        };
    };
    let normalized_units = if eq_ascii(units, "manual") {
        ascii_units("default")
    } else {
        units.to_vec()
    };
    if !NATIVE_PERMISSION_MODES
        .iter()
        .any(|allowed| ascii_units(allowed).as_slice() == normalized_units.as_slice())
    {
        return NativePermissionDefaultModeResult {
            status: NativePermissionDefaultModeStatus::Invalid,
            original: Some(original),
            normalized: None,
            due_issue: Some(native_default_mode_due_issue()),
        };
    }
    NativePermissionDefaultModeResult {
        status: NativePermissionDefaultModeStatus::Valid,
        original: Some(original),
        normalized: Some(string_projection(normalized_units)),
        due_issue: None,
    }
}

fn native_default_mode_due_issue() -> NativePermissionDueIssue {
    let mut parts = vec![TextPart::Static("Invalid option: expected one of ")];
    for (index, mode) in NATIVE_PERMISSION_MODES.iter().enumerate() {
        if index > 0 {
            parts.push(TextPart::Static("|"));
        }
        parts.push(TextPart::Static("\""));
        parts.push(TextPart::Static(mode));
        parts.push(TextPart::Static("\""));
    }
    NativePermissionDueIssue {
        path: "permissions.defaultMode",
        message: string_projection(compose_units(&parts)),
    }
}

/// Source `fxe(e, bucket)` over one exact JavaScript string.
#[must_use]
pub(crate) fn validate_native_permission_rule(
    rule_units: &[u16],
    class: NativePermissionRuleClass,
    original: Utf16JsonProjection,
) -> NativePermissionRuleResult {
    let parsed = parse_wue(rule_units);
    let mut result = NativePermissionRuleResult {
        original,
        class,
        syntax: parsed.kind,
        tool_name: parsed.tool_name.clone(),
        raw_content: parsed.raw_content.clone(),
        normalized_tool_name: None,
        normalized_content: None,
        status: NativePermissionRuleStatus::Valid,
        kept: None,
        error: None,
        suggestion: None,
        examples: Vec::new(),
        warning: None,
    };

    if rule_units.is_empty() || trim_range(rule_units).is_empty() {
        return invalid(
            &mut result,
            ascii_units("Permission rule cannot be empty"),
            None,
            Vec::new(),
        );
    }
    if parsed.kind == NativePermissionSyntaxKind::Malformed {
        return invalid(
            &mut result,
            ascii_units("Malformed Tool(content) rule"),
            Some(ascii_units(
                r#"Rules take the form Tool(content) and must end at the closing ")"; parentheses inside the content are literal"#,
            )),
            Vec::new(),
        );
    }
    if parsed.kind == NativePermissionSyntaxKind::Call
        && parsed.raw_content.as_deref() == Some(&[][..])
    {
        let tool_name = parsed.tool_name.as_deref().unwrap_or(&[]);
        if tool_name.is_empty() {
            return invalid(
                &mut result,
                ascii_units("Empty parentheses with no tool name"),
                Some(ascii_units("Specify a tool name before the parentheses")),
                Vec::new(),
            );
        }
        return invalid(
            &mut result,
            ascii_units("Empty parentheses"),
            Some(compose_units(&[
                TextPart::Static("Either specify a pattern or use just \""),
                TextPart::Units(tool_name),
                TextPart::Static("\" without parentheses"),
            ])),
            vec![
                tool_name.to_vec(),
                compose_units(&[
                    TextPart::Units(tool_name),
                    TextPart::Static("(some-pattern)"),
                ]),
            ],
        );
    }

    let (tool_name, content) = normalized_rule_parts(rule_units, &parsed);
    result.normalized_tool_name = Some(tool_name.clone());
    result.normalized_content = content.clone();

    let mcp = parse_mcp_tool_name(&tool_name);
    if let Some(mcp) = mcp {
        if parsed.kind == NativePermissionSyntaxKind::Call {
            let server_pattern = mcp.server_name.clone();
            let mut examples = vec![
                compose_units(&[TextPart::Static("mcp__"), TextPart::Units(&server_pattern)]),
                compose_units(&[
                    TextPart::Static("mcp__"),
                    TextPart::Units(&server_pattern),
                    TextPart::Static("__*"),
                ]),
            ];
            if let Some(tool) = mcp.tool_name.as_deref() {
                if !tool.is_empty() && !eq_ascii(tool, "*") {
                    examples.push(compose_units(&[
                        TextPart::Static("mcp__"),
                        TextPart::Units(&server_pattern),
                        TextPart::Static("__"),
                        TextPart::Units(tool),
                    ]));
                }
            }
            return invalid(
                &mut result,
                ascii_units("MCP rules do not support patterns in parentheses"),
                Some(compose_units(&[
                    TextPart::Static("Use \""),
                    TextPart::Units(&tool_name),
                    TextPart::Static("\" without parentheses, or use \"mcp__"),
                    TextPart::Units(&server_pattern),
                    TextPart::Static("__*\" for all tools"),
                ])),
                examples,
            );
        }
        if class.is_allow() {
            if let Some((error, suggestion, examples)) = wildcard_tool_name_error(&tool_name) {
                return invalid(&mut result, error, Some(suggestion), examples);
            }
        }
        return result;
    }

    if tool_name.is_empty() {
        return invalid(
            &mut result,
            ascii_units("Tool name cannot be empty"),
            None,
            Vec::new(),
        );
    }
    if class.is_allow() {
        if let Some((error, suggestion, examples)) = wildcard_tool_name_error(&tool_name) {
            return invalid(&mut result, error, Some(suggestion), examples);
        }
    }

    if !contains_ascii(&tool_name, "_") {
        match uppercase_check(&tool_name) {
            UppercaseCheck::AlreadyUppercase => {}
            UppercaseCheck::Changed(uppercase_first) => {
                let suggestion = uppercase_suggestion(&tool_name, &uppercase_first);
                return invalid(
                    &mut result,
                    ascii_units("Tool names must start with uppercase"),
                    Some(compose_units(&[
                        TextPart::Static("Use \""),
                        TextPart::Units(&suggestion),
                        TextPart::Static("\""),
                    ])),
                    Vec::new(),
                );
            }
        }
    }

    if parsed.kind == NativePermissionSyntaxKind::Call
        && parsed
            .raw_content
            .as_deref()
            .is_some_and(ambiguous_escaped_parentheses)
    {
        let file_tool = is_file_pattern_tool(&tool_name);
        let suggestion = if file_tool {
            ascii_units(
                "Write a Windows path with forward slashes, or spell a literal parenthesis as [(] or [)]",
            )
        } else {
            ascii_units(
                r#"Double a backslash that is a path separator ("\\("), and escape literal parentheses in pairs"#,
            )
        };
        let examples = if file_tool {
            vec![
                compose_units(&[
                    TextPart::Units(&tool_name),
                    TextPart::Static("(C:/Projects/(drafts)/**)"),
                ]),
                compose_units(&[
                    TextPart::Units(&tool_name),
                    TextPart::Static(r"(C:\Projects\[(]drafts)\**)"),
                ]),
            ]
        } else {
            vec![compose_units(&[
                TextPart::Units(&tool_name),
                TextPart::Static(r"(C:\tools\\(x86)\run.exe *)"),
            ])]
        };
        return invalid(
            &mut result,
            ascii_units(r#"Ambiguous "\(" or "\)" beside an unescaped parenthesis"#),
            Some(suggestion),
            examples,
        );
    }

    if let Some(content) = content.as_deref() {
        if let Some((error, suggestion, examples)) = custom_content_validator(&tool_name, content) {
            return invalid(&mut result, error, Some(suggestion), examples);
        }
    }

    if is_bash_prefix_tool(&tool_name) {
        if let Some(content) = content.as_deref() {
            if eq_ascii(content, ":*") {
                return invalid(
                    &mut result,
                    ascii_units("Prefix cannot be empty before :*"),
                    Some(ascii_units("Specify a command prefix before :*")),
                    vec![ascii_units("Bash(npm *)"), ascii_units("Bash(git *)")],
                );
            }
            if class.is_allow() {
                if let Some(prefix) = bash_wildcard_prefix(content) {
                    let is_git = eq_ascii(&prefix, "git");
                    let mut warning = compose_units(&[
                        TextPart::Units(&format_rule(&tool_name, Some(content))),
                        TextPart::Static(
                            " has a wildcard before the rest of the command, so it also matches any options inserted at that position and approves them without a prompt.",
                        ),
                    ]);
                    if is_git {
                        warning.extend(ascii_units(
                            " For git, options such as -c and --exec-path can run arbitrary commands.",
                        ));
                    }
                    warning.extend(ascii_units(
                        " Replace that * with the exact value you mean, or only use * after the subcommand",
                    ));
                    if is_git {
                        warning.extend(ascii_units(" (for example Bash(git status *))"));
                    }
                    warning.push(b'.' as u16);
                    result.warning = Some(string_projection(warning));
                    return result;
                }
            }

            if let Some(prefix) = trailing_bash_prefix(content) {
                if has_unescaped_wildcard(&prefix) {
                    let literal_star = has_unescaped_trailing_star(&prefix);
                    let outcome = if literal_star {
                        "matches only commands containing a literal * at that position"
                    } else {
                        "will likely never match"
                    };
                    let mut warning = compose_units(&[
                        TextPart::Units(&format_rule(&tool_name, Some(content))),
                        TextPart::Static(
                            " mixes * with the trailing :* prefix syntax, so it is matched as a literal prefix (the * is not expanded) and ",
                        ),
                        TextPart::Static(outcome),
                        TextPart::Static("."),
                    ]);
                    if class.is_allow() {
                        warning.extend(ascii_units(
                            " Replace that * with the exact value you mean.",
                        ));
                    } else if !ends_with_ascii(&prefix, ":*") {
                        warning.extend(compose_units(&[
                            TextPart::Static(" Use "),
                            TextPart::Units(&format_rule(
                                &tool_name,
                                Some(&append_ascii(&prefix, "*")),
                            )),
                            TextPart::Static(" for wildcard matching."),
                        ]));
                    }
                    result.warning = Some(string_projection(warning));
                    return result;
                }
            } else if contains_ascii(content, ":*") && !ends_with_ascii(content, ":*") {
                let advice = if class.is_allow() {
                    "Replace that :* with the exact value you mean."
                } else {
                    "It already matches as a * wildcard; moving :* to the end would make it a literal prefix and change which commands match."
                };
                result.warning = Some(string_projection(compose_units(&[
                    TextPart::Units(&format_rule(&tool_name, Some(content))),
                    TextPart::Static(
                        " has a :* that is not at the end, so it is matched as a * wildcard (the : is literal), not as the trailing :* prefix syntax. ",
                    ),
                    TextPart::Static(advice),
                ])));
                return result;
            }
        }
    }

    if is_file_pattern_tool(&tool_name) {
        if let Some(content) = content.as_deref() {
            if contains_ascii(content, ":*") {
                return invalid(
                    &mut result,
                    ascii_units("The \":*\" syntax is only for Bash prefix rules"),
                    Some(ascii_units(
                        "Use glob patterns like \"*\" or \"**\" for file matching",
                    )),
                    vec![
                        compose_units(&[
                            TextPart::Units(&tool_name),
                            TextPart::Static("(*.ts) - matches .ts files"),
                        ]),
                        compose_units(&[
                            TextPart::Units(&tool_name),
                            TextPart::Static("(src/**) - matches all files in src"),
                        ]),
                        compose_units(&[
                            TextPart::Units(&tool_name),
                            TextPart::Static("(**/*.test.ts) - matches test files"),
                        ]),
                    ],
                );
            }
        }
    }

    if let Some(content) = content.as_deref() {
        let replacement_tool = if eq_ascii(&tool_name, "Write")
            || eq_ascii(&tool_name, "NotebookEdit")
            || eq_ascii(&tool_name, "MultiEdit")
        {
            Some("Edit")
        } else if eq_ascii(&tool_name, "Glob") {
            Some("Read")
        } else {
            None
        };
        if let Some(replacement_tool) = replacement_tool {
            if !contains_ascii(content, ":*") {
                let file_action = if replacement_tool == "Edit" {
                    "editing"
                } else {
                    "reading"
                };
                result.warning = Some(string_projection(compose_units(&[
                    TextPart::Units(&format_rule(&tool_name, Some(content))),
                    TextPart::Static(" is not matched by file permission checks — only "),
                    TextPart::Static(replacement_tool),
                    TextPart::Static("(path) rules are. Use "),
                    TextPart::Units(&format_rule(
                        replacement_tool
                            .encode_utf16()
                            .collect::<Vec<_>>()
                            .as_slice(),
                        Some(content),
                    )),
                    TextPart::Static(" instead ("),
                    TextPart::Static(replacement_tool),
                    TextPart::Static(" rules cover all file-"),
                    TextPart::Static(file_action),
                    TextPart::Static(" tools)."),
                ])));
            }
        }
    }

    result
}

#[derive(Debug, Clone)]
struct WueResult {
    kind: NativePermissionSyntaxKind,
    tool_name: Option<Vec<u16>>,
    raw_content: Option<Vec<u16>>,
}

fn parse_wue(value: &[u16]) -> WueResult {
    if !value.contains(&(b'(' as u16)) && !value.contains(&(b')' as u16)) {
        return WueResult {
            kind: NativePermissionSyntaxKind::Bare,
            tool_name: None,
            raw_content: None,
        };
    }
    let open = find_unescaped(value, b'(' as u16, false);
    let close = find_unescaped(value, b')' as u16, true);
    if open.is_none() && close.is_none() {
        return WueResult {
            kind: NativePermissionSyntaxKind::Bare,
            tool_name: None,
            raw_content: None,
        };
    }
    let open_index = open.unwrap_or(0);
    let prefix = if open.is_some() {
        &value[..open_index]
    } else {
        &[]
    };
    let malformed = open.is_none()
        || close.is_none()
        || close.is_some_and(|index| index <= open_index || index + 1 != value.len())
        || prefix
            .iter()
            .any(|unit| *unit == b'(' as u16 || *unit == b')' as u16);
    if malformed {
        return WueResult {
            kind: NativePermissionSyntaxKind::Malformed,
            tool_name: None,
            raw_content: None,
        };
    }
    let close_index = close.expect("checked above");
    WueResult {
        kind: NativePermissionSyntaxKind::Call,
        tool_name: Some(prefix.to_vec()),
        raw_content: Some(value[open_index + 1..close_index].to_vec()),
    }
}

fn normalized_rule_parts(value: &[u16], parsed: &WueResult) -> (Vec<u16>, Option<Vec<u16>>) {
    let Some(tool_name) = parsed.tool_name.as_deref() else {
        return (alias_tool_name(value), None);
    };
    let Some(content) = parsed.raw_content.as_deref() else {
        return (alias_tool_name(value), None);
    };
    if tool_name.is_empty() {
        return (alias_tool_name(value), None);
    }
    if content.is_empty() || eq_ascii(content, "*") {
        return (alias_tool_name(tool_name), None);
    }
    (
        alias_tool_name(tool_name),
        Some(normalize_rule_content(content)),
    )
}

fn alias_tool_name(value: &[u16]) -> Vec<u16> {
    for (source, target) in [
        ("Task", "Agent"),
        ("KillShell", "TaskStop"),
        ("KillBash", "TaskStop"),
        ("ListPeers", "ListAgents"),
        ("Brief", "SendUserMessage"),
        ("ListMcpResources", "ListMcpResourcesTool"),
        ("ReadMcpResource", "ReadMcpResourceTool"),
        ("ReadMcpResourceDir", "ReadMcpResourceDirTool"),
    ] {
        if eq_ascii(value, source) {
            return ascii_units(target);
        }
    }
    value.to_vec()
}

fn normalize_rule_content(value: &[u16]) -> Vec<u16> {
    let open = replace_units(value, &[b'\\' as u16, b'(' as u16], &[b'(' as u16]);
    let close = replace_units(&open, &[b'\\' as u16, b')' as u16], &[b')' as u16]);
    replace_units(&close, &[b'\\' as u16, b'\\' as u16], &[b'\\' as u16])
}

fn format_rule(tool_name: &[u16], content: Option<&[u16]>) -> Vec<u16> {
    let Some(content) = content.filter(|content| !content.is_empty()) else {
        return tool_name.to_vec();
    };
    let escaped = replace_units(content, &[b'\\' as u16], &[b'\\' as u16, b'\\' as u16]);
    let escaped = replace_units(&escaped, &[b'(' as u16], &[b'\\' as u16, b'(' as u16]);
    let escaped = replace_units(&escaped, &[b')' as u16], &[b'\\' as u16, b')' as u16]);
    compose_units(&[
        TextPart::Units(tool_name),
        TextPart::Static("("),
        TextPart::Units(&escaped),
        TextPart::Static(")"),
    ])
}

#[derive(Debug)]
struct McpToolName {
    server_name: Vec<u16>,
    tool_name: Option<Vec<u16>>,
}

fn parse_mcp_tool_name(value: &[u16]) -> Option<McpToolName> {
    let separator = ascii_units("__");
    let parts = split_subsequence(value, &separator);
    if parts.len() < 2 || !eq_ascii(parts[0], "mcp") || parts[1].is_empty() {
        return None;
    }
    Some(McpToolName {
        server_name: parts[1].to_vec(),
        tool_name: (parts.len() > 2).then(|| {
            let mut joined = Vec::new();
            for (index, part) in parts[2..].iter().enumerate() {
                if index > 0 {
                    joined.extend_from_slice(&separator);
                }
                joined.extend_from_slice(part);
            }
            joined
        }),
    })
}

fn wildcard_tool_name_error(value: &[u16]) -> Option<(Vec<u16>, Vec<u16>, Vec<Vec<u16>>)> {
    if !contains_ascii(value, "*") {
        return None;
    }
    let mcp = parse_mcp_tool_name(value);
    if mcp
        .as_ref()
        .is_some_and(|parsed| !contains_ascii(&parsed.server_name, "*"))
    {
        return None;
    }
    Some((
        compose_units(&[
            TextPart::Static("Wildcard tool name \""),
            TextPart::Units(value),
            TextPart::Static("\" is not supported in allow rules"),
        ]),
        ascii_units(
            "An allow pattern must name the scope it widens — globs are permitted only in the tool position after a literal mcp__<server>__ prefix. Deny and ask rules accept wildcards anywhere",
        ),
        vec![
            ascii_units("mcp__puppeteer__*"),
            ascii_units("mcp__github__get_*"),
        ],
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum UppercaseCheck {
    AlreadyUppercase,
    Changed(Vec<u16>),
}

fn uppercase_check(tool_name: &[u16]) -> UppercaseCheck {
    let Some(first) = tool_name.first().copied() else {
        return UppercaseCheck::AlreadyUppercase;
    };
    let uppercase = js_uppercase_first_unit(first);
    if uppercase.as_slice() == [first] {
        UppercaseCheck::AlreadyUppercase
    } else {
        UppercaseCheck::Changed(uppercase)
    }
}

/// Exact `Sa(String(toolName))`: uppercase the first UTF-16 code unit and
/// append the untouched remainder, whose indexing is also by code unit.
fn uppercase_suggestion(tool_name: &[u16], uppercase_first: &[u16]) -> Vec<u16> {
    let mut suggestion =
        Vec::with_capacity(uppercase_first.len() + tool_name.len().saturating_sub(1));
    suggestion.extend_from_slice(uppercase_first);
    suggestion.extend_from_slice(tool_name.get(1..).unwrap_or_default());
    suggestion
}

// Generated by enumerating String.fromCharCode(codeUnit).toUpperCase() in Bun 1.4.3 / Unicode 16.0.
// Runtime SHA-256: 28c7dd0a06916aba284d570ef44a2d70e9c538eaf2ba58dfe0910525563b3f79.
// Canonical changed-code-unit JSON SHA-256: 935c53fd3dd439df94ee71791f9d8f95136cac0276a3e29a0.
// The sparse mapping has 1,270 changed inputs, including 102 multi-unit expansions;
// a separate exhaustive comparison verified all 65,536 inputs against the runtime.
// Identity code units (including lone surrogates) are returned unchanged.
fn js_uppercase_first_unit(unit: u16) -> Vec<u16> {
    match unit {
        0xAB70..=0xABBF => vec![(i32::from(unit) + (-38864)) as u16],
        0x2C65 => vec![(i32::from(unit) + (-10795)) as u16],
        0x2C66 => vec![(i32::from(unit) + (-10792)) as u16],
        0x2D00..=0x2D25 | 0x2D27 | 0x2D2D => vec![(i32::from(unit) + (-7264)) as u16],
        0x1FBE => vec![(i32::from(unit) + (-7205)) as u16],
        0x1C80 => vec![(i32::from(unit) + (-6254)) as u16],
        0x1C81 => vec![(i32::from(unit) + (-6253)) as u16],
        0x1C82 => vec![(i32::from(unit) + (-6244)) as u16],
        0x1C85 => vec![(i32::from(unit) + (-6243)) as u16],
        0x1C83 | 0x1C84 => vec![(i32::from(unit) + (-6242)) as u16],
        0x1C86 => vec![(i32::from(unit) + (-6236)) as u16],
        0x1C87 => vec![(i32::from(unit) + (-6181)) as u16],
        0xAB53 => vec![(i32::from(unit) + (-928)) as u16],
        0x017F => vec![(i32::from(unit) + (-300)) as u16],
        0x0131 => vec![(i32::from(unit) + (-232)) as u16],
        0x0292 => vec![(i32::from(unit) + (-219)) as u16],
        0x0280 | 0x0283 | 0x0288 => vec![(i32::from(unit) + (-218)) as u16],
        0x028A | 0x028B => vec![(i32::from(unit) + (-217)) as u16],
        0x0275 => vec![(i32::from(unit) + (-214)) as u16],
        0x0272 => vec![(i32::from(unit) + (-213)) as u16],
        0x0269 | 0x026F => vec![(i32::from(unit) + (-211)) as u16],
        0x0253 => vec![(i32::from(unit) + (-210)) as u16],
        0x0268 => vec![(i32::from(unit) + (-209)) as u16],
        0x0263 => vec![(i32::from(unit) + (-207)) as u16],
        0x0254 => vec![(i32::from(unit) + (-206)) as u16],
        0x0256 | 0x0257 | 0x0260 => vec![(i32::from(unit) + (-205)) as u16],
        0x025B => vec![(i32::from(unit) + (-203)) as u16],
        0x0259 => vec![(i32::from(unit) + (-202)) as u16],
        0x03F3 => vec![(i32::from(unit) + (-116)) as u16],
        0x03F5 => vec![(i32::from(unit) + (-96)) as u16],
        0x03F0 => vec![(i32::from(unit) + (-86)) as u16],
        0x0450..=0x045F | 0x03F1 => vec![(i32::from(unit) + (-80)) as u16],
        0x01DD => vec![(i32::from(unit) + (-79)) as u16],
        0x028C => vec![(i32::from(unit) + (-71)) as u16],
        0x0289 => vec![(i32::from(unit) + (-69)) as u16],
        0x03CC => vec![(i32::from(unit) + (-64)) as u16],
        0x03CD | 0x03CE => vec![(i32::from(unit) + (-63)) as u16],
        0x03D0 => vec![(i32::from(unit) + (-62)) as u16],
        0x1E9B => vec![(i32::from(unit) + (-59)) as u16],
        0x03D1 => vec![(i32::from(unit) + (-57)) as u16],
        0x03D6 => vec![(i32::from(unit) + (-54)) as u16],
        0x0561..=0x0586 | 0x2C30..=0x2C5F => vec![(i32::from(unit) + (-48)) as u16],
        0x03D5 => vec![(i32::from(unit) + (-47)) as u16],
        0x03AC => vec![(i32::from(unit) + (-38)) as u16],
        0x03AD..=0x03AF => vec![(i32::from(unit) + (-37)) as u16],
        0x0061..=0x007A
        | 0x00E0..=0x00F6
        | 0x00F8..=0x00FE
        | 0x03B1..=0x03C1
        | 0x03C3..=0x03CB
        | 0x0430..=0x044F
        | 0xFF41..=0xFF5A => vec![(i32::from(unit) + (-32)) as u16],
        0x03C2 => vec![(i32::from(unit) + (-31)) as u16],
        0x214E => vec![(i32::from(unit) + (-28)) as u16],
        0x24D0..=0x24E9 => vec![(i32::from(unit) + (-26)) as u16],
        0x2170..=0x217F => vec![(i32::from(unit) + (-16)) as u16],
        0x04CF => vec![(i32::from(unit) + (-15)) as u16],
        0x13F8..=0x13FD | 0x03D7 => vec![(i32::from(unit) + (-8)) as u16],
        0x01F3 => vec![(i32::from(unit) + (-2)) as u16],
        0x0183 | 0x0185 | 0x0188 | 0x018C | 0x0192 | 0x0199 | 0x01A8 | 0x01AD | 0x01B0 | 0x01B4
        | 0x01B6 | 0x01B9 | 0x01BD | 0x01F2 | 0x01F5 | 0x023C | 0x0242 | 0x0371 | 0x0373
        | 0x0377 | 0x03F8 | 0x03FB | 0x1C8A | 0x2184 | 0x2C61 | 0x2C73 | 0x2C76 | 0x2CEC
        | 0x2CEE | 0x2CF3 | 0xA77A | 0xA77C | 0xA78C | 0xA791 | 0xA793 | 0xA7C8 | 0xA7CA
        | 0xA7CD | 0xA7D1 | 0xA7F6 => vec![(i32::from(unit) + (-1)) as u16],
        0x03F2 | 0x1FE5 => vec![(i32::from(unit) + (7)) as u16],
        0x1F00..=0x1F07
        | 0x1F10..=0x1F15
        | 0x1F20..=0x1F27
        | 0x1F30..=0x1F37
        | 0x1F40..=0x1F45
        | 0x1F60..=0x1F67
        | 0x1FB0
        | 0x1FB1
        | 0x1FD0
        | 0x1FD1
        | 0x1FE0
        | 0x1FE1 => vec![(i32::from(unit) + (8)) as u16],
        0xA794 => vec![(i32::from(unit) + (48)) as u16],
        0x01BF => vec![(i32::from(unit) + (56)) as u16],
        0x1F70 | 0x1F71 => vec![(i32::from(unit) + (74)) as u16],
        0x0345 => vec![(i32::from(unit) + (84)) as u16],
        0x1F72..=0x1F75 => vec![(i32::from(unit) + (86)) as u16],
        0x0195 => vec![(i32::from(unit) + (97)) as u16],
        0x1F76 | 0x1F77 => vec![(i32::from(unit) + (100)) as u16],
        0x1F7A | 0x1F7B => vec![(i32::from(unit) + (112)) as u16],
        0x00FF => vec![(i32::from(unit) + (121)) as u16],
        0x1F7C | 0x1F7D => vec![(i32::from(unit) + (126)) as u16],
        0x1F78 | 0x1F79 => vec![(i32::from(unit) + (128)) as u16],
        0x037B..=0x037D | 0x019E => vec![(i32::from(unit) + (130)) as u16],
        0x019A => vec![(i32::from(unit) + (163)) as u16],
        0x0180 => vec![(i32::from(unit) + (195)) as u16],
        0x00B5 => vec![(i32::from(unit) + (743)) as u16],
        0x10D0..=0x10FA | 0x10FD..=0x10FF => vec![(i32::from(unit) + (3008)) as u16],
        0x1D7D => vec![(i32::from(unit) + (3814)) as u16],
        0x027D => vec![(i32::from(unit) + (10727)) as u16],
        0x026B => vec![(i32::from(unit) + (10743)) as u16],
        0x0271 => vec![(i32::from(unit) + (10749)) as u16],
        0x0251 => vec![(i32::from(unit) + (10780)) as u16],
        0x0252 => vec![(i32::from(unit) + (10782)) as u16],
        0x0250 => vec![(i32::from(unit) + (10783)) as u16],
        0x023F | 0x0240 => vec![(i32::from(unit) + (10815)) as u16],
        0x1C88 => vec![(i32::from(unit) + (35266)) as u16],
        0x1D79 => vec![(i32::from(unit) + (35332)) as u16],
        0x1D8E => vec![(i32::from(unit) + (35384)) as u16],
        0x029E => vec![(i32::from(unit) + (42258)) as u16],
        0x029D => vec![(i32::from(unit) + (42261)) as u16],
        0x0265 => vec![(i32::from(unit) + (42280)) as u16],
        0x0287 => vec![(i32::from(unit) + (42282)) as u16],
        0x026C => vec![(i32::from(unit) + (42305)) as u16],
        0x0282 => vec![(i32::from(unit) + (42307)) as u16],
        0x0266 | 0x026A => vec![(i32::from(unit) + (42308)) as u16],
        0x0261 => vec![(i32::from(unit) + (42315)) as u16],
        0x025C => vec![(i32::from(unit) + (42319)) as u16],
        0x0264 => vec![(i32::from(unit) + (42343)) as u16],
        0x019B => vec![(i32::from(unit) + (42561)) as u16],
        0x01C6..=0x01CC if unit % 3 == 1 => vec![(i32::from(unit) + (-2)) as u16],
        0x013A..=0x0148 | 0x017A..=0x017E | 0x01D0..=0x01DC | 0x04C2..=0x04CE | 0x2C68..=0x2C6C
            if unit.is_multiple_of(2) =>
        {
            vec![(i32::from(unit) + (-1)) as u16]
        }
        0x0101..=0x012F
        | 0x0133..=0x0137
        | 0x014B..=0x0177
        | 0x01A1..=0x01A5
        | 0x01DF..=0x01EF
        | 0x01F9..=0x021F
        | 0x0223..=0x0233
        | 0x0247..=0x024F
        | 0x03D9..=0x03EF
        | 0x0461..=0x0481
        | 0x048B..=0x04BF
        | 0x04D1..=0x052F
        | 0x1E01..=0x1E95
        | 0x1EA1..=0x1EFF
        | 0x2C81..=0x2CE3
        | 0xA641..=0xA66D
        | 0xA681..=0xA69B
        | 0xA723..=0xA72F
        | 0xA733..=0xA76F
        | 0xA77F..=0xA787
        | 0xA797..=0xA7A9
        | 0xA7B5..=0xA7C3
        | 0xA7D7..=0xA7DB
            if unit % 2 == 1 =>
        {
            vec![(i32::from(unit) + (-1)) as u16]
        }
        0x01C5..=0x01CE if unit.is_multiple_of(3) => vec![(i32::from(unit) + (-1)) as u16],
        0x1F51..=0x1F57 if unit % 2 == 1 => vec![(i32::from(unit) + (8)) as u16],
        0x00DF => vec![0x0053, 0x0053],
        0x0149 => vec![0x02BC, 0x004E],
        0x01F0 => vec![0x004A, 0x030C],
        0x0390 => vec![0x0399, 0x0308, 0x0301],
        0x03B0 => vec![0x03A5, 0x0308, 0x0301],
        0x0587 => vec![0x0535, 0x0552],
        0x1E96 => vec![0x0048, 0x0331],
        0x1E97 => vec![0x0054, 0x0308],
        0x1E98 => vec![0x0057, 0x030A],
        0x1E99 => vec![0x0059, 0x030A],
        0x1E9A => vec![0x0041, 0x02BE],
        0x1F50 => vec![0x03A5, 0x0313],
        0x1F52 => vec![0x03A5, 0x0313, 0x0300],
        0x1F54 => vec![0x03A5, 0x0313, 0x0301],
        0x1F56 => vec![0x03A5, 0x0313, 0x0342],
        0x1F80 => vec![0x1F08, 0x0399],
        0x1F81 => vec![0x1F09, 0x0399],
        0x1F82 => vec![0x1F0A, 0x0399],
        0x1F83 => vec![0x1F0B, 0x0399],
        0x1F84 => vec![0x1F0C, 0x0399],
        0x1F85 => vec![0x1F0D, 0x0399],
        0x1F86 => vec![0x1F0E, 0x0399],
        0x1F87 => vec![0x1F0F, 0x0399],
        0x1F88 => vec![0x1F08, 0x0399],
        0x1F89 => vec![0x1F09, 0x0399],
        0x1F8A => vec![0x1F0A, 0x0399],
        0x1F8B => vec![0x1F0B, 0x0399],
        0x1F8C => vec![0x1F0C, 0x0399],
        0x1F8D => vec![0x1F0D, 0x0399],
        0x1F8E => vec![0x1F0E, 0x0399],
        0x1F8F => vec![0x1F0F, 0x0399],
        0x1F90 => vec![0x1F28, 0x0399],
        0x1F91 => vec![0x1F29, 0x0399],
        0x1F92 => vec![0x1F2A, 0x0399],
        0x1F93 => vec![0x1F2B, 0x0399],
        0x1F94 => vec![0x1F2C, 0x0399],
        0x1F95 => vec![0x1F2D, 0x0399],
        0x1F96 => vec![0x1F2E, 0x0399],
        0x1F97 => vec![0x1F2F, 0x0399],
        0x1F98 => vec![0x1F28, 0x0399],
        0x1F99 => vec![0x1F29, 0x0399],
        0x1F9A => vec![0x1F2A, 0x0399],
        0x1F9B => vec![0x1F2B, 0x0399],
        0x1F9C => vec![0x1F2C, 0x0399],
        0x1F9D => vec![0x1F2D, 0x0399],
        0x1F9E => vec![0x1F2E, 0x0399],
        0x1F9F => vec![0x1F2F, 0x0399],
        0x1FA0 => vec![0x1F68, 0x0399],
        0x1FA1 => vec![0x1F69, 0x0399],
        0x1FA2 => vec![0x1F6A, 0x0399],
        0x1FA3 => vec![0x1F6B, 0x0399],
        0x1FA4 => vec![0x1F6C, 0x0399],
        0x1FA5 => vec![0x1F6D, 0x0399],
        0x1FA6 => vec![0x1F6E, 0x0399],
        0x1FA7 => vec![0x1F6F, 0x0399],
        0x1FA8 => vec![0x1F68, 0x0399],
        0x1FA9 => vec![0x1F69, 0x0399],
        0x1FAA => vec![0x1F6A, 0x0399],
        0x1FAB => vec![0x1F6B, 0x0399],
        0x1FAC => vec![0x1F6C, 0x0399],
        0x1FAD => vec![0x1F6D, 0x0399],
        0x1FAE => vec![0x1F6E, 0x0399],
        0x1FAF => vec![0x1F6F, 0x0399],
        0x1FB2 => vec![0x1FBA, 0x0399],
        0x1FB3 => vec![0x0391, 0x0399],
        0x1FB4 => vec![0x0386, 0x0399],
        0x1FB6 => vec![0x0391, 0x0342],
        0x1FB7 => vec![0x0391, 0x0342, 0x0399],
        0x1FBC => vec![0x0391, 0x0399],
        0x1FC2 => vec![0x1FCA, 0x0399],
        0x1FC3 => vec![0x0397, 0x0399],
        0x1FC4 => vec![0x0389, 0x0399],
        0x1FC6 => vec![0x0397, 0x0342],
        0x1FC7 => vec![0x0397, 0x0342, 0x0399],
        0x1FCC => vec![0x0397, 0x0399],
        0x1FD2 => vec![0x0399, 0x0308, 0x0300],
        0x1FD3 => vec![0x0399, 0x0308, 0x0301],
        0x1FD6 => vec![0x0399, 0x0342],
        0x1FD7 => vec![0x0399, 0x0308, 0x0342],
        0x1FE2 => vec![0x03A5, 0x0308, 0x0300],
        0x1FE3 => vec![0x03A5, 0x0308, 0x0301],
        0x1FE4 => vec![0x03A1, 0x0313],
        0x1FE6 => vec![0x03A5, 0x0342],
        0x1FE7 => vec![0x03A5, 0x0308, 0x0342],
        0x1FF2 => vec![0x1FFA, 0x0399],
        0x1FF3 => vec![0x03A9, 0x0399],
        0x1FF4 => vec![0x038F, 0x0399],
        0x1FF6 => vec![0x03A9, 0x0342],
        0x1FF7 => vec![0x03A9, 0x0342, 0x0399],
        0x1FFC => vec![0x03A9, 0x0399],
        0xFB00 => vec![0x0046, 0x0046],
        0xFB01 => vec![0x0046, 0x0049],
        0xFB02 => vec![0x0046, 0x004C],
        0xFB03 => vec![0x0046, 0x0046, 0x0049],
        0xFB04 => vec![0x0046, 0x0046, 0x004C],
        0xFB05 => vec![0x0053, 0x0054],
        0xFB06 => vec![0x0053, 0x0054],
        0xFB13 => vec![0x0544, 0x0546],
        0xFB14 => vec![0x0544, 0x0535],
        0xFB15 => vec![0x0544, 0x053B],
        0xFB16 => vec![0x054E, 0x0546],
        0xFB17 => vec![0x0544, 0x053D],
        _ => vec![unit],
    }
}

fn ambiguous_escaped_parentheses(content: &[u16]) -> bool {
    let has_backslash_paren_pattern = (0..content.len().saturating_sub(1)).any(|index| {
        content[index] == b'\\' as u16
            && (content[index + 1] == b'(' as u16 || content[index + 1] == b')' as u16)
            && (index == 0 || content[index - 1] != b'\\' as u16)
    });
    has_backslash_paren_pattern
        && count_unescaped(content, b'(' as u16) != count_unescaped(content, b')' as u16)
}

fn count_unescaped(value: &[u16], target: u16) -> usize {
    (0..value.len())
        .filter(|index| value[*index] == target && !is_escaped(value, *index))
        .count()
}

fn is_escaped(value: &[u16], index: usize) -> bool {
    let mut cursor = index;
    let mut backslashes = 0usize;
    while cursor > 0 && value[cursor - 1] == b'\\' as u16 {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}

fn find_unescaped(value: &[u16], target: u16, reverse: bool) -> Option<usize> {
    if reverse {
        (0..value.len())
            .rev()
            .find(|index| value[*index] == target && !is_escaped(value, *index))
    } else {
        (0..value.len()).find(|index| value[*index] == target && !is_escaped(value, *index))
    }
}

fn custom_content_validator(
    tool_name: &[u16],
    content: &[u16],
) -> Option<(Vec<u16>, Vec<u16>, Vec<Vec<u16>>)> {
    if eq_ascii(tool_name, "WebSearch") {
        if contains_ascii(content, "*") || contains_ascii(content, "?") {
            return Some((
                ascii_units("WebSearch does not support wildcards"),
                ascii_units("Use exact search terms without * or ?"),
                vec![
                    ascii_units("WebSearch(claude ai)"),
                    ascii_units("WebSearch(typescript tutorial)"),
                ],
            ));
        }
    } else if eq_ascii(tool_name, "WebFetch") {
        if contains_ascii(content, "://") || starts_with_ascii(content, "http") {
            return Some((
                ascii_units("WebFetch permissions use domain format, not URLs"),
                ascii_units("Use \"domain:hostname\" format"),
                vec![
                    ascii_units("WebFetch(domain:example.com)"),
                    ascii_units("WebFetch(domain:github.com)"),
                ],
            ));
        }
        if !starts_with_ascii(content, "domain:") {
            return Some((
                ascii_units("WebFetch permissions must use \"domain:\" prefix"),
                ascii_units("Use \"domain:hostname\" format"),
                vec![
                    ascii_units("WebFetch(domain:example.com)"),
                    ascii_units("WebFetch(domain:*.google.com)"),
                ],
            ));
        }
    }
    None
}

fn is_bash_prefix_tool(tool_name: &[u16]) -> bool {
    eq_ascii(tool_name, "Bash")
}

fn is_file_pattern_tool(tool_name: &[u16]) -> bool {
    [
        "Read",
        "Write",
        "Edit",
        "Glob",
        "NotebookRead",
        "NotebookEdit",
        "Cd",
    ]
    .iter()
    .any(|tool| eq_ascii(tool_name, tool))
}

fn is_shell_operator_token(token: &[u16]) -> bool {
    if token.first().is_some_and(|unit| {
        [
            b'|' as u16,
            b'&' as u16,
            b';' as u16,
            b'<' as u16,
            b'>' as u16,
        ]
        .contains(unit)
    }) {
        return true;
    }
    let digits = token
        .iter()
        .take_while(|unit| (b'0' as u16..=b'9' as u16).contains(unit))
        .count();
    digits > 0
        && token
            .get(digits)
            .is_some_and(|unit| *unit == b'<' as u16 || *unit == b'>' as u16)
}

fn bash_wildcard_prefix(content: &[u16]) -> Option<Vec<u16>> {
    if ends_with_ascii(content, ":*") {
        return None;
    }
    let trimmed = trim_range(content);
    let words = split_ecma_whitespace(trimmed);
    if words.len() < 3
        || words
            .first()
            .is_none_or(|word| has_unescaped_wildcard(word))
    {
        return None;
    }
    let mut saw_wildcard = false;
    for word in words.iter().skip(1) {
        if is_shell_operator_token(word) {
            return None;
        }
        if has_unescaped_wildcard(word) {
            saw_wildcard = true;
            continue;
        }
        if word.first() == Some(&(b'-' as u16)) {
            continue;
        }
        return saw_wildcard.then(|| words[0].to_vec());
    }
    None
}

fn trailing_bash_prefix(content: &[u16]) -> Option<Vec<u16>> {
    if content.len() < 3 || !ends_with_ascii(content, ":*") {
        return None;
    }
    let prefix = &content[..content.len() - 2];
    if prefix
        .iter()
        .any(|unit| matches!(*unit, 0x000a | 0x000d | 0x2028 | 0x2029))
    {
        return None;
    }
    (!prefix.is_empty()).then(|| trim_end_units(prefix).to_vec())
}

fn has_unescaped_wildcard(value: &[u16]) -> bool {
    (0..value.len()).any(|index| value[index] == b'*' as u16 && !is_escaped(value, index))
}

fn has_unescaped_trailing_star(value: &[u16]) -> bool {
    let trimmed = trim_end_units(value);
    if trimmed.last() != Some(&(b'*' as u16)) {
        return false;
    }
    !is_escaped(trimmed, trimmed.len() - 1)
}

fn replace_units(value: &[u16], needle: &[u16], replacement: &[u16]) -> Vec<u16> {
    if needle.is_empty() {
        return value.to_vec();
    }
    let mut result = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index..].starts_with(needle) {
            result.extend_from_slice(replacement);
            index += needle.len();
        } else {
            result.push(value[index]);
            index += 1;
        }
    }
    result
}

fn split_subsequence<'a>(value: &'a [u16], separator: &[u16]) -> Vec<&'a [u16]> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + separator.len() <= value.len() {
        if value[index..].starts_with(separator) {
            parts.push(&value[start..index]);
            index += separator.len();
            start = index;
        } else {
            index += 1;
        }
    }
    parts.push(&value[start..]);
    parts
}

fn split_ecma_whitespace(value: &[u16]) -> Vec<&[u16]> {
    let mut parts = Vec::new();
    let mut start = None;
    for (index, unit) in value.iter().copied().enumerate() {
        if is_ecma_whitespace(unit) {
            if let Some(start) = start.take() {
                parts.push(&value[start..index]);
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(start) = start {
        parts.push(&value[start..]);
    }
    parts
}

fn trim_range(value: &[u16]) -> &[u16] {
    let start = value
        .iter()
        .position(|unit| !is_ecma_whitespace(*unit))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|unit| !is_ecma_whitespace(*unit))
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn trim_end_units(value: &[u16]) -> &[u16] {
    let end = value
        .iter()
        .rposition(|unit| !is_ecma_whitespace(*unit))
        .map_or(0, |index| index + 1);
    &value[..end]
}

fn is_ecma_whitespace(unit: u16) -> bool {
    matches!(
        unit,
        0x0009
            | 0x000b
            | 0x000c
            | 0x0020
            | 0x00a0
            | 0x000a
            | 0x000d
            | 0x2028
            | 0x2029
            | 0xfeff
            | 0x1680
            | 0x2000..=0x200a | 0x202f | 0x205f | 0x3000
    )
}

fn invalid(
    result: &mut NativePermissionRuleResult,
    error: Vec<u16>,
    suggestion: Option<Vec<u16>>,
    examples: Vec<Vec<u16>>,
) -> NativePermissionRuleResult {
    result.status = NativePermissionRuleStatus::Invalid;
    result.error = Some(string_projection(error));
    result.suggestion = suggestion.map(string_projection);
    result.examples = examples.into_iter().map(string_projection).collect();
    result.clone()
}

fn contains_ascii(value: &[u16], needle: &str) -> bool {
    let needle = ascii_units(needle);
    contains_units(value, &needle)
}

fn contains_units(value: &[u16], needle: &[u16]) -> bool {
    if needle.is_empty() {
        return true;
    }
    value.windows(needle.len()).any(|window| window == needle)
}

fn starts_with_ascii(value: &[u16], prefix: &str) -> bool {
    value.starts_with(&ascii_units(prefix))
}

fn ends_with_ascii(value: &[u16], suffix: &str) -> bool {
    value.ends_with(&ascii_units(suffix))
}

fn eq_ascii(value: &[u16], expected: &str) -> bool {
    value == ascii_units(expected).as_slice()
}

fn ascii_units(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

enum TextPart<'a> {
    Static(&'static str),
    Units(&'a [u16]),
}

fn compose_units(parts: &[TextPart<'_>]) -> Vec<u16> {
    let capacity = parts
        .iter()
        .map(|part| match part {
            TextPart::Static(value) => value.encode_utf16().count(),
            TextPart::Units(value) => value.len(),
        })
        .sum();
    let mut result = Vec::with_capacity(capacity);
    for part in parts {
        match part {
            TextPart::Static(value) => result.extend(value.encode_utf16()),
            TextPart::Units(value) => result.extend_from_slice(value),
        }
    }
    result
}

fn append_ascii(value: &[u16], suffix: &str) -> Vec<u16> {
    let suffix_units = suffix.encode_utf16().collect::<Vec<_>>();
    compose_units(&[TextPart::Units(value), TextPart::Units(&suffix_units)])
}

fn string_projection(units: Vec<u16>) -> Utf16JsonProjection {
    let display = String::from_utf16_lossy(&units);
    Utf16JsonProjection::root_string(display, units)
        .expect("the display spelling is derived from these exact UTF-16 units")
}

fn projection_from_view(view: JsonValueView<'_>) -> Utf16JsonProjection {
    enum Task<'a> {
        Visit(JsonValueView<'a>),
        Array(usize),
        Object(Vec<Vec<u16>>),
    }

    let mut builder = Utf16JsonBuilder::new();
    let mut tasks = vec![Task::Visit(view)];
    let mut completed = Vec::<usize>::new();
    while let Some(task) = tasks.pop() {
        match task {
            Task::Visit(view) => {
                if let Some(units) = view.code_units() {
                    completed.push(builder.push_string_units(units.to_vec()));
                } else if let Some(length) = view.array_len() {
                    tasks.push(Task::Array(length));
                    for index in (0..length).rev() {
                        if let Some(child) = view.get_index(index) {
                            tasks.push(Task::Visit(child));
                        }
                    }
                } else if let Some(entries) = view.object_entries() {
                    let keys = entries
                        .iter()
                        .map(|entry| entry.code_units.to_vec())
                        .collect::<Vec<_>>();
                    tasks.push(Task::Object(keys));
                    for entry in entries.into_iter().rev() {
                        tasks.push(Task::Visit(entry.value));
                    }
                } else if view.is_null() {
                    completed.push(builder.push_null());
                } else if let Some(value) = view.as_bool() {
                    completed.push(builder.push_bool(value));
                } else if let Some(value) = view.as_number() {
                    completed.push(builder.push_number(value));
                } else {
                    unreachable!("every JSON projection node has a JSON value kind")
                }
            }
            Task::Array(length) => {
                let children = completed.split_off(completed.len() - length);
                completed.push(
                    builder
                        .push_array(children)
                        .expect("copied array references earlier builder nodes"),
                );
            }
            Task::Object(keys) => {
                let children = completed.split_off(completed.len() - keys.len());
                let fields = keys.into_iter().zip(children).collect();
                completed.push(
                    builder
                        .push_object_units(fields)
                        .expect("copied object references earlier builder nodes"),
                );
            }
        }
    }
    builder
        .finish(completed.pop().expect("one root projection was copied"))
        .expect("copied projection is a valid flat JSON tree")
}

fn apply_object_changes(
    source: &Utf16JsonProjection,
    replacements: &[(Vec<u16>, Utf16JsonProjection)],
    removals: &[Vec<u16>],
) -> Utf16JsonProjection {
    let Some(root) = source.at_pointer("") else {
        return source.clone();
    };
    let Some(entries) = root.object_entries() else {
        return source.clone();
    };
    let mut output = Vec::with_capacity(entries.len());
    for entry in entries {
        if removals.iter().any(|key| key == entry.code_units) {
            continue;
        }
        let value = replacements
            .iter()
            .find(|(key, _)| key == entry.code_units)
            .map(|(_, value)| value.clone())
            .unwrap_or_else(|| projection_from_view(entry.value));
        output.push((entry.code_units.to_vec(), value));
    }
    Utf16JsonProjection::object_units(output)
        .expect("source-ordered object edits retain valid unique UTF-16 keys")
}

fn compose_reasons(reasons: &[String]) -> Vec<u16> {
    let mut output = Vec::new();
    for (index, reason) in reasons.iter().enumerate() {
        if index > 0 {
            output.extend(ascii_units(" and "));
        }
        output.extend(reason.encode_utf16());
    }
    output
}

fn unresolved_root_result(projection: Utf16JsonProjection) -> NativePermissionResult {
    NativePermissionResult {
        normalized: projection,
        buckets: Vec::new(),
        diagnostics: Vec::new(),
        withheld: Vec::new(),
        default_mode: NativePermissionDefaultModeResult {
            status: NativePermissionDefaultModeStatus::Absent,
            original: None,
            normalized: None,
            due_issue: None,
        },
        unresolved_fields: vec!["permissions".to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> NativePermissionSourceContext {
        NativePermissionSourceContext {
            file: "managedSettings".to_owned(),
            policy_source: true,
            strict_parse_follows: true,
        }
    }

    fn validate(text: &str, class: NativePermissionRuleClass) -> NativePermissionRuleResult {
        let original =
            Utf16JsonProjection::root_string(text.to_owned(), text.encode_utf16().collect())
                .expect("ordinary source rule string");
        validate_native_permission_rule(
            text.encode_utf16().collect::<Vec<_>>().as_slice(),
            class,
            original,
        )
    }

    #[test]
    fn positive_invalid_mcp_and_shell_rules_keep_native_order_and_advisories() {
        assert_eq!(
            validate("Read(src/**)", NativePermissionRuleClass::Allow).status,
            NativePermissionRuleStatus::Valid
        );
        assert_eq!(
            validate("Read()", NativePermissionRuleClass::Allow).status,
            NativePermissionRuleStatus::Invalid
        );
        assert_eq!(
            validate("mcp__github__get_issue", NativePermissionRuleClass::Allow).status,
            NativePermissionRuleStatus::Valid
        );
        let pattern = validate(
            "mcp__github__get_issue(pattern)",
            NativePermissionRuleClass::Ask,
        );
        assert_eq!(pattern.status, NativePermissionRuleStatus::Invalid);
        assert!(pattern
            .error
            .as_ref()
            .and_then(|value| value.string_at(""))
            .is_some_and(|message| message.contains("MCP rules do not support patterns")));
        // Native only warns when a wildcard appears before a later argument.
        // `Bash(git status *)` is the source's safer after-subcommand example.
        let shell = validate("Bash(git status *)", NativePermissionRuleClass::Allow);
        assert_eq!(shell.status, NativePermissionRuleStatus::Valid);
        assert!(shell.warning.is_none());

        let shell_with_option_gap =
            validate("Bash(git * status)", NativePermissionRuleClass::Allow);
        assert_eq!(
            shell_with_option_gap.status,
            NativePermissionRuleStatus::Valid
        );
        assert_eq!(
            shell_with_option_gap
                .warning
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("Bash(git * status) has a wildcard before the rest of the command, so it also matches any options inserted at that position and approves them without a prompt. For git, options such as -c and --exec-path can run arbitrary commands. Replace that * with the exact value you mean, or only use * after the subcommand (for example Bash(git status *))."),
        );
    }

    #[test]
    fn bash_wildcard_advisory_matches_native_qg_first_non_option_and_eof() {
        // Fixed Native qg returns at the first non-option argument; reaching
        // the loop end always returns undefined, even after seeing a star.
        for (command, prefix) in [
            ("git status *", None),
            ("git * status", Some("git")),
            ("git status -- *", None),
            ("git -C * status", Some("git")),
            ("git * --version", None),
        ] {
            let units = command.encode_utf16().collect::<Vec<_>>();
            assert_eq!(
                super::bash_wildcard_prefix(&units),
                prefix.map(|value| value.encode_utf16().collect::<Vec<_>>()),
                "Native qg advisory for {command:?}",
            );
            let result = validate(
                &format!("Bash({command})"),
                NativePermissionRuleClass::Allow,
            );
            assert_eq!(result.status, NativePermissionRuleStatus::Valid);
            assert_eq!(result.warning.is_some(), prefix.is_some());
        }
    }

    #[test]
    fn deny_ask_all_invalid_floor_grants_but_partial_trim_does_not() {
        let full = Utf16JsonProjection::parse(
            r#"{"allow":["Read(src/**)"],"deny":["Read()"],"additionalDirectories":["/tmp"],"defaultMode":"acceptEdits"}"#,
        )
        .expect("permissions fixture");
        let result = normalize_native_permissions(&full, &context());
        assert_eq!(
            result
                .buckets
                .iter()
                .find(|bucket| bucket.field == "deny")
                .map(|bucket| bucket.status),
            Some(NativePermissionArrayStatus::Unreadable)
        );
        assert_eq!(
            result.normalized.to_json_string().expect("valid JSON"),
            r#"{"deny":[],"defaultMode":"default"}"#
        );
        assert_eq!(
            result
                .withheld
                .iter()
                .map(|entry| entry.field)
                .collect::<Vec<_>>(),
            vec!["allow", "additionalDirectories", "defaultMode"]
        );

        let partial = Utf16JsonProjection::parse(
            r#"{"allow":["Read(src/**)"],"deny":["Read()","Edit(src/**)"],"defaultMode":"acceptEdits"}"#,
        )
        .expect("permissions fixture");
        let result = normalize_native_permissions(&partial, &context());
        assert_eq!(
            result
                .buckets
                .iter()
                .find(|bucket| bucket.field == "deny")
                .map(|bucket| bucket.status),
            Some(NativePermissionArrayStatus::Trimmed)
        );
        assert!(result.withheld.is_empty());
    }

    #[test]
    fn manual_default_mode_is_normalized_and_nonstring_entries_are_warned_in_order() {
        let input = Utf16JsonProjection::parse(
            r#"{"allow":["Read(src/**)",7,"Read()"],"defaultMode":"manual"}"#,
        )
        .expect("permissions fixture");
        let result = normalize_native_permissions(&input, &context());
        assert_eq!(
            result.normalized.to_json_string().expect("valid JSON"),
            r#"{"allow":["Read(src/**)"],"defaultMode":"default"}"#
        );
        assert_eq!(result.diagnostics.len(), 2);
        assert_eq!(
            result.diagnostics[0].kind,
            NativePermissionDiagnosticKind::NonStringEntryRemoved
        );
        assert_eq!(
            result.diagnostics[1].kind,
            NativePermissionDiagnosticKind::InvalidRuleSkipped
        );
    }

    #[test]
    fn default_mode_uses_only_the_source_bound_six_mode_enum() {
        assert_eq!(
            NATIVE_PERMISSION_MODES,
            [
                "acceptEdits",
                "auto",
                "bypassPermissions",
                "default",
                "dontAsk",
                "plan",
            ]
        );
        for mode in NATIVE_PERMISSION_MODES {
            let value =
                Utf16JsonProjection::root_string(mode.to_owned(), mode.encode_utf16().collect())
                    .expect("Native default mode string");
            assert_eq!(
                normalize_default_mode(&value).status,
                NativePermissionDefaultModeStatus::Valid
            );
        }
        let provider_only = Utf16JsonProjection::root_string(
            "providerMode".to_owned(),
            "providerMode".encode_utf16().collect(),
        )
        .expect("non-native provider mode fixture");
        assert_eq!(
            normalize_default_mode(&provider_only).status,
            NativePermissionDefaultModeStatus::Invalid
        );
        let issue = normalize_default_mode(&provider_only)
            .due_issue
            .expect("invalid enum value produces the native Zod issue");
        assert_eq!(issue.path, "permissions.defaultMode");
        assert_eq!(
            issue.message.string_at(""),
            Some(
                "Invalid option: expected one of \"acceptEdits\"|\"auto\"|\"bypassPermissions\"|\"default\"|\"dontAsk\"|\"plan\""
            )
        );

        let null_value = Utf16JsonProjection::parse("null").expect("JSON null");
        let null_issue = normalize_default_mode(&null_value)
            .due_issue
            .expect("null reaches the Zod enum and gets an invalid-value issue");
        assert_eq!(null_issue.path, issue.path);
        assert_eq!(null_issue.message, issue.message);
    }

    #[test]
    fn first_utf16_unit_matches_bun_unicode16_expansions_and_surrogates() {
        let expanded = validate("ßash", NativePermissionRuleClass::Deny);
        assert_eq!(expanded.status, NativePermissionRuleStatus::Invalid);
        assert_eq!(
            expanded
                .suggestion
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("Use \"SSash\"")
        );

        let dotless_i = validate("ıTool", NativePermissionRuleClass::Deny);
        assert_eq!(dotless_i.status, NativePermissionRuleStatus::Invalid);
        assert_eq!(
            dotless_i
                .suggestion
                .as_ref()
                .and_then(|value| value.string_at("")),
            Some("Use \"ITool\"")
        );

        let mut lone_surrogate = vec![0xd800];
        lone_surrogate.extend("Tool".encode_utf16());
        let surrogate_rule = string_projection(lone_surrogate.clone());
        assert_eq!(
            validate_native_permission_rule(
                &lone_surrogate,
                NativePermissionRuleClass::Deny,
                surrogate_rule,
            )
            .status,
            NativePermissionRuleStatus::Valid
        );

        assert_eq!(
            validate("Readé", NativePermissionRuleClass::Deny).status,
            NativePermissionRuleStatus::Valid
        );
        assert_eq!(
            validate("ß_tool", NativePermissionRuleClass::Deny).status,
            NativePermissionRuleStatus::Valid
        );
    }

    #[test]
    fn lone_surrogate_rule_content_is_retained_as_utf16() {
        let input = Utf16JsonProjection::parse(r#""Read(\ud800)""#)
            .expect("escaped lone surrogate is valid JSON input");
        let units = input
            .at_pointer("")
            .and_then(JsonValueView::code_units)
            .expect("root string")
            .to_vec();
        let result =
            validate_native_permission_rule(&units, NativePermissionRuleClass::Allow, input);
        assert_eq!(result.status, NativePermissionRuleStatus::Valid);
        assert_eq!(result.raw_content, Some(vec![0xd800]));
    }
}
