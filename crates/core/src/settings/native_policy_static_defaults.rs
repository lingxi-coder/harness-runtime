//! Source-derived implementation of the fixed 2.1.295 `fTr` settings path.
//!
//! The caller admits static `defaultSettings` with I5e before this module;
//! dynamic helper output enters at fTr after the `Hi()` envelope check. This
//! module keeps source, ordered `At`, and normalized `Due` projections apart.
//! A present field whose At or Due behavior is not represented remains
//! unresolved and can never make a candidate `Validated`.
//!
//! Native source: `src_187673145.js`, binary base 187673145, size 434959,
//! SHA-256 `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`.
//! `fTr` local 371765; `At` 387398; `Nue` 311541; `f_` 378184; `d_` 373362;
//! `u_` 374361; `k_` 380323; `v_` 381597; `A_` 382711; `R_` 384162;
//! `P_` 386152; `m_` 377479; `__` 379580; `h_` 378260. The caller passes
//! `policySource:true` and `mcpServerEntrySalvageOnly:true`.

use super::native_extra_known_marketplaces::{
    normalize_native_extra_known_marketplaces, validate_native_extra_known_marketplaces_due,
};
use super::native_managed_mcp_servers::{
    normalize_native_managed_mcp_servers, NativeManagedMcpServerDiagnostic,
};
use super::native_permission_rule_validation::{
    normalize_native_permissions, NativePermissionResult, NativePermissionSourceContext,
};
use super::native_policy::NativePolicySemanticEvidence;
use super::native_policy_hook_settings::{
    normalize_native_hook_settings, validate_native_hooks_due, NativeHookSourceContext,
};
use super::native_policy_js_semantics::NativePolicyJsSemanticsRegistry;
use super::native_policy_marketplaces::normalize_policy_marketplace_field;
use super::native_policy_parser::{normalize_mcp_server_entry, static_helper_settings_issue};
use super::native_policy_regex::NativePolicyRegexValidationRow;
use super::native_policy_strict_settings::{
    collect_native_regex_patterns, string_projection, string_projection_units,
    validate_native_strict_settings, NativeStrictSettingsIssue, NATIVE_DUE_FIELD_ORDER,
};
use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

const NATIVE_PROVIDER_NAMES: &[&str] = &[
    "anthropic",
    "customEndpoint",
    "bedrock",
    "vertex",
    "foundry",
    "anthropicAws",
    "mantle",
    "gateway",
];

/// Result class after applying I5e, `At`, the two fTr hard gates, `Due`, and
/// the final `O_` projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeStaticSettingsStatus {
    Valid,
    Invalid,
    Unresolved,
}

/// Ordered source events from the tolerant `At` transforms. These retain the
/// offending value instead of fabricating a Rust-formatted Native warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeStaticAtEvent {
    pub field: String,
    pub kind: NativeStaticAtEventKind,
    pub index: Option<usize>,
    pub raw_value: Option<Utf16JsonProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeStaticAtEventKind {
    AliasIgnored,
    AliasNullRemoved,
    RestrictiveNullRemoved,
    PermissionResult,
    ModelPickerDropped,
    ModelPickerOptionDropped,
    ModelPickerReplaceFlagDropped,
    CrossSessionInboundRestricted,
    RemoteControlRestricted,
    ExtraMarketplaceFieldDropped,
    ExtraMarketplaceResultApplied,
    ModelPricingRootDropped,
    ModelPricingMultiplierDropped,
    ModelPricingOverridesDropped,
    ModelPricingOverrideDropped,
    McpServerEntryDropped,
    ManagedMcpServerDiagnostic,
}

/// The fTr hard checks are kept distinct from ordinary Due issues because
/// Native runs them in this order before constructing/parsing the Due object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NativeStaticHardRejection {
    StrictPluginOnlyCustomization {
        raw_value: Utf16JsonProjection,
    },
    AllowedProvidersNotArray {
        raw_value: Utf16JsonProjection,
    },
    AllowedProvidersNotStrings {
        raw_value: Utf16JsonProjection,
        index: usize,
    },
    AllowedProvidersUnknownName {
        raw_value: Utf16JsonProjection,
        rejected_names: Vec<Utf16JsonProjection>,
    },
}

/// The three representations correspond to Native's `s`, `f.data`, and
/// `O_(s, f.data)` in `fTr`. `raw` is never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeStaticSettingsResult {
    pub raw: Utf16JsonProjection,
    pub after_at: Option<Utf16JsonProjection>,
    pub due_projection: Option<Utf16JsonProjection>,
    pub normalized: Option<Utf16JsonProjection>,
    pub at_events: Vec<NativeStaticAtEvent>,
    /// Exact Native warning rows returned by `At`/`fTr`, before any static
    /// fallback wrapper adds `statusOnly:true`.
    pub at_warnings: Vec<Utf16JsonProjection>,
    /// Exact warning rows returned by Native `tr` for static candidates.
    pub static_fallback_warnings: Vec<Utf16JsonProjection>,
    /// Triggered warnings whose exact raw source row is not yet represented.
    pub warning_unresolved_fields: Vec<String>,
    pub permission_result: Option<NativePermissionResult>,
    /// Exact `qt` callback rows before Native `__` adds its path and message
    /// prefixes; callback paths/messages remain UTF-16 projections.
    pub managed_mcp_diagnostics: Vec<NativeManagedMcpServerDiagnostic>,
    /// Unique first-seen JS RegExp patterns Native reaches in this `Due` pass.
    /// Runtime validates these in the pinned Bun worker before rerunning the
    /// same pure Core path with the returned result rows.
    pub regex_patterns: Vec<Vec<u16>>,
    /// Exact I5e custom issue when the helper's static-payload gate rejects.
    pub i5e_issue: Option<NativeStrictSettingsIssue>,
    pub hard_rejection: Option<NativeStaticHardRejection>,
    pub due_issues: Vec<NativeStrictSettingsIssue>,
    pub unresolved_fields: Vec<String>,
    pub status: NativeStaticSettingsStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeStaticSettingsDiagnosticKind {
    I5eStartupFatal,
    AtHardRejection,
    DueRejection,
}

/// A source-formatted Native diagnostic. The projection is a root JavaScript
/// string, so paths or JSON-stringified values containing lone UTF-16 units
/// stay lossless until the final host display boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeStaticSettingsDiagnostic {
    pub kind: NativeStaticSettingsDiagnosticKind,
    pub message: Utf16JsonProjection,
}

/// Exact inner `fTr` rejection consumed by `tr()`. This is also the error
/// returned by dynamic helper `qn()` without a static-fallback wrapper.
pub(crate) fn native_ftr_settings_error(
    result: &NativeStaticSettingsResult,
    source_field: &str,
) -> Option<Utf16JsonProjection> {
    if let Some(issue) = &result.i5e_issue {
        return Some(issue.message.clone());
    }

    if let Some(rejection) = &result.hard_rejection {
        let ftr_context = if source_field.starts_with("policyHelpers.") {
            "payload"
        } else {
            "managedSettings"
        };
        let reason = format_hard_rejection(rejection)?;
        return Some(string_projection(&format!(
            "{ftr_context} rejected: {reason}"
        )));
    }

    if result.due_issues.is_empty() || !result.unresolved_fields.is_empty() {
        return None;
    }
    let ftr_context = if source_field.starts_with("policyHelpers.") {
        "payload"
    } else {
        "managedSettings"
    };
    let details = result
        .due_issues
        .iter()
        .map(format_due_issue_units)
        .collect::<Option<Vec<_>>>()?;
    let prefix = format!("{ftr_context} rejected: ");
    let mut message_units = prefix.encode_utf16().collect::<Vec<_>>();
    for (index, detail) in details.iter().enumerate() {
        if index > 0 {
            message_units.extend("; ".encode_utf16());
        }
        message_units.extend_from_slice(detail);
    }
    Some(string_projection_units(message_units))
}

/// Return the exact messages passed to Native's `qn()` logger for this
/// result. The caller decides whether the source call site actually uses
/// `qn()` (dynamic helper output always does; `tr()` does only for the first
/// on-chain static fallback candidate). This models the side effect without
/// pretending that every static candidate logs its warnings.
#[cfg(test)]
pub(crate) fn native_qn_log_messages(
    result: &NativeStaticSettingsResult,
    source_field: &str,
) -> Option<Vec<Utf16JsonProjection>> {
    native_qn_log_messages_from_source(&result.raw, &result.at_warnings, source_field)
}

/// Build the exact messages emitted by Native `qn()` from the original source
/// projection and its exact At warning rows. Static `tr()` rows carry the same
/// warning shape with `statusOnly:true`, so this helper serves both paths.
pub(crate) fn native_qn_log_messages_from_source(
    raw: &Utf16JsonProjection,
    warnings: &[Utf16JsonProjection],
    source_field: &str,
) -> Option<Vec<Utf16JsonProjection>> {
    let mut messages = Vec::new();
    if raw
        .at_pointer("")
        .is_some_and(|root| root.object_entries().is_some())
    {
        for field in ["policyHelper", "policyHelpers"] {
            if let Ok(value) = raw.subprojection(&format!("/{field}")) {
                if !value.at_pointer("").is_some_and(JsonValueView::is_null) {
                    messages.push(string_projection(&format!(
                        "{source_field}: stripped {field} from the settings payload (no recursion)"
                    )));
                }
            }
        }
    }

    for warning in warnings {
        let warning_message_projection = warning.subprojection("/message").ok()?;
        let warning_message = warning_message_projection.at_pointer("")?.code_units()?;
        let mut units = source_field.encode_utf16().collect::<Vec<_>>();
        units.extend(": ".encode_utf16());
        units.extend_from_slice(warning_message);
        messages.push(string_projection_units(units));
    }
    Some(messages)
}

/// Exact `xi()` startup fatal for one invalid local static candidate. Native
/// calls this after validating every `er()` candidate, including off-chain
/// candidates. It is deliberately separate from the helper startupFatal
/// diagnostics emitted during settings loading.
pub(crate) fn native_static_settings_diagnostic(
    result: &NativeStaticSettingsResult,
    source_field: &str,
) -> Option<NativeStaticSettingsDiagnostic> {
    let kind = if result.i5e_issue.is_some() {
        NativeStaticSettingsDiagnosticKind::I5eStartupFatal
    } else if result.hard_rejection.is_some() {
        NativeStaticSettingsDiagnosticKind::AtHardRejection
    } else if !result.due_issues.is_empty() {
        NativeStaticSettingsDiagnosticKind::DueRejection
    } else {
        return None;
    };
    let reason = native_ftr_settings_error(result, source_field)?;
    let reason_units = reason.at_pointer("")?.code_units()?;
    let mut message_units = source_field.encode_utf16().collect::<Vec<_>>();
    message_units.extend(" is not a valid static settings payload (".encode_utf16());
    message_units.extend_from_slice(reason_units);
    message_units.extend("); Claude Code will not start until it is fixed".encode_utf16());
    Some(NativeStaticSettingsDiagnostic {
        kind,
        message: string_projection_units(message_units),
    })
}

fn format_due_issue_units(issue: &NativeStrictSettingsIssue) -> Option<Vec<u16>> {
    let path = issue.path.at_pointer("")?.code_units()?;
    let message = issue.message.at_pointer("")?.code_units()?;
    let mut units = Vec::with_capacity(path.len() + message.len() + 2);
    if !path.is_empty() {
        units.extend_from_slice(path);
        units.extend([u16::from(b':'), u16::from(b' ')]);
    }
    units.extend_from_slice(message);
    Some(units)
}

fn dotted_native_issue_path(path: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let root = path.at_pointer("")?;
    let length = root.array_len()?;
    let mut units = Vec::new();
    for index in 0..length {
        if index > 0 {
            units.push(u16::from(b'.'));
        }
        let segment = path.subprojection(&format!("/{index}")).ok()?;
        let segment = segment.at_pointer("")?;
        if let Some(value) = segment.code_units() {
            units.extend_from_slice(value);
        } else if let Some(value) = segment.as_number() {
            if !value.is_finite() {
                return None;
            }
            units.extend(value.to_string().encode_utf16());
        } else {
            return None;
        }
    }
    Some(string_projection_units(units))
}

fn static_fallback_warning(warning: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let root = warning.at_pointer("")?;
    let entries = root.object_entries()?;
    let status_key = "statusOnly".encode_utf16().collect::<Vec<_>>();
    let status_value = Utf16JsonProjection::parse("true").ok()?;
    let mut fields = Vec::with_capacity(entries.len() + 1);
    let mut had_status = false;
    for entry in entries {
        let value = if entry.code_units == status_key {
            had_status = true;
            status_value.clone()
        } else {
            warning
                .subprojection_property_units(entry.code_units)
                .ok()?
        };
        fields.push((entry.code_units.to_vec(), value));
    }
    if !had_status {
        fields.push((status_key, status_value));
    }
    Utf16JsonProjection::object_units(fields).ok()
}

fn native_warning_row(
    file: &str,
    path_units: &[u16],
    message_units: &[u16],
    mut extra_fields: Vec<(Vec<u16>, Utf16JsonProjection)>,
) -> Option<Utf16JsonProjection> {
    let mut fields = vec![
        ("file".encode_utf16().collect(), string_projection(file)),
        (
            "path".encode_utf16().collect(),
            string_projection_units(path_units.to_vec()),
        ),
        (
            "message".encode_utf16().collect(),
            string_projection_units(message_units.to_vec()),
        ),
        (
            "severity".encode_utf16().collect(),
            string_projection("warning"),
        ),
    ];
    fields.append(&mut extra_fields);
    Utf16JsonProjection::object_units(fields).ok()
}

fn append_permission_warnings(
    file: &str,
    result: &NativePermissionResult,
    warnings: &mut Vec<Utf16JsonProjection>,
    unresolved: &mut Vec<String>,
) {
    for diagnostic in &result.diagnostics {
        let Some(message) = diagnostic
            .message
            .as_ref()
            .and_then(|value| value.at_pointer(""))
            .and_then(JsonValueView::code_units)
        else {
            push_unique(unresolved, "permissions");
            continue;
        };
        let mut extra_fields = Vec::new();
        if let Some(invalid_value) = &diagnostic.invalid_value {
            extra_fields.push((
                "invalidValue".encode_utf16().collect(),
                invalid_value.clone(),
            ));
        }
        if diagnostic.substituted {
            extra_fields.push((
                "substituted".encode_utf16().collect(),
                Utf16JsonProjection::parse("true").expect("true is valid JSON"),
            ));
        }
        if let Some(warning) = native_warning_row(
            file,
            &diagnostic.path.encode_utf16().collect::<Vec<_>>(),
            message,
            extra_fields,
        ) {
            warnings.push(warning);
        } else {
            push_unique(unresolved, "permissions");
        }
    }
}

fn managed_mcp_warning(
    file: &str,
    diagnostic: &NativeManagedMcpServerDiagnostic,
) -> Option<Utf16JsonProjection> {
    let callback_path = diagnostic.callback_path.at_pointer("")?.code_units()?;
    let callback_message = diagnostic.message.at_pointer("")?.code_units()?;
    let (path, message) = if callback_path.is_empty() {
        (
            "managedMcpServers".encode_utf16().collect::<Vec<_>>(),
            callback_message.to_vec(),
        )
    } else {
        let mut path = "managedMcpServers.".encode_utf16().collect::<Vec<_>>();
        path.extend_from_slice(callback_path);
        let mut message = "Managed MCP server was ignored: "
            .encode_utf16()
            .collect::<Vec<_>>();
        message.extend_from_slice(callback_message);
        (path, message)
    };
    native_warning_row(file, &path, &message, Vec::new())
}

fn record_at_warning_gaps(events: &[NativeStaticAtEvent], unresolved: &mut Vec<String>) {
    for event in events {
        match event.kind {
            NativeStaticAtEventKind::AliasIgnored
                if matches!(
                    event.field.as_str(),
                    "additionalMarketplaces" | "allowedMarketplaces"
                ) => {}
            NativeStaticAtEventKind::AliasIgnored
            | NativeStaticAtEventKind::AliasNullRemoved
            | NativeStaticAtEventKind::RestrictiveNullRemoved
            | NativeStaticAtEventKind::PermissionResult
            | NativeStaticAtEventKind::ManagedMcpServerDiagnostic => {}
            NativeStaticAtEventKind::ModelPickerDropped
            | NativeStaticAtEventKind::ModelPickerOptionDropped
            | NativeStaticAtEventKind::ModelPickerReplaceFlagDropped => {
                push_unique(unresolved, "modelPicker");
            }
            NativeStaticAtEventKind::CrossSessionInboundRestricted => {
                push_unique(unresolved, "crossSessionInbound");
            }
            NativeStaticAtEventKind::RemoteControlRestricted => {
                push_unique(unresolved, "remoteControl");
            }
            // The exact k_ warning row is retained in at_warnings; a dropped
            // field alone is no longer an unformatted-warning gap.
            NativeStaticAtEventKind::ExtraMarketplaceFieldDropped
            | NativeStaticAtEventKind::ExtraMarketplaceResultApplied => {}
            NativeStaticAtEventKind::ModelPricingRootDropped
            | NativeStaticAtEventKind::ModelPricingMultiplierDropped
            | NativeStaticAtEventKind::ModelPricingOverridesDropped
            | NativeStaticAtEventKind::ModelPricingOverrideDropped => {
                push_unique(unresolved, "modelPricing");
            }
            NativeStaticAtEventKind::McpServerEntryDropped => {
                push_unique(unresolved, &event.field);
            }
        }
    }
}

fn format_hard_rejection(rejection: &NativeStaticHardRejection) -> Option<String> {
    match rejection {
        NativeStaticHardRejection::StrictPluginOnlyCustomization { raw_value } => Some(format!(
            "strictPluginOnlyCustomization: must be true, false, or an array of surface names; received {}",
            native_received_type(raw_value.at_pointer("")?)
        )),
        NativeStaticHardRejection::AllowedProvidersNotArray { raw_value } => Some(format!(
            "allowedProviders: must be an array of provider names; received {}",
            native_received_type(raw_value.at_pointer("")?)
        )),
        NativeStaticHardRejection::AllowedProvidersNotStrings { raw_value, index } => {
            let item = raw_value.subprojection(&format!("/{index}")).ok()?;
            Some(format!(
                "allowedProviders: entries must be provider names (strings); received {}",
                native_received_type(item.at_pointer("")?)
            ))
        }
        NativeStaticHardRejection::AllowedProvidersUnknownName { rejected_names, .. } => {
            let names = rejected_names
                .iter()
                .map(|value| value.to_json_string())
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            let noun = if names.len() == 1 { "name" } else { "names" };
            Some(format!(
                "allowedProviders: unknown provider {noun} {}; known names: {}",
                names.join(", "),
                NATIVE_PROVIDER_NAMES.join(", ")
            ))
        }
    }
}

fn native_received_type(value: JsonValueView<'_>) -> &'static str {
    if value.is_null() {
        "null"
    } else if value.array_len().is_some() {
        "array"
    } else if value.code_units().is_some() {
        "string"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if let Some(number) = value.as_number() {
        if number.is_nan() {
            "NaN"
        } else if number == f64::INFINITY {
            "Infinity"
        } else if number == f64::NEG_INFINITY {
            "-Infinity"
        } else {
            "number"
        }
    } else {
        "object"
    }
}

/// Run the static default-settings acceptance path.
///
/// The permission rule validator uses the fixed Native mode set. In this
/// 2.1.295 build, `fH` contains six modes and the only `Hdt()` mode extension
/// is empty because all five build gates are true and `HE` is a copy of `fH`.
pub(crate) fn validate_native_static_default_settings(
    raw: &Utf16JsonProjection,
    source_field: &str,
    semantics: &NativePolicySemanticEvidence,
) -> NativeStaticSettingsResult {
    // `tr` only runs I5e for a non-null, non-array object, after destructuring
    // the two nested helper keys away. `fTr` itself also strips those keys,
    // so retain the original raw projection for the shared normalizer below.
    if raw
        .at_pointer("")
        .is_some_and(|root| !root.is_null() && root.object_entries().is_some())
    {
        let mut i5e_input = raw.clone();
        for field in ["policyHelper", "policyHelpers"] {
            let _ = i5e_input.remove_field(field);
        }
        if let Some(issue) = static_helper_settings_issue(&i5e_input) {
            return invalid_result(
                raw,
                Some(NativeStrictSettingsIssue {
                    path: string_projection(source_field),
                    message: string_projection(&issue),
                }),
            );
        }
    }

    let mut result = validate_native_helper_managed_settings(raw, source_field, semantics);
    for warning in &result.at_warnings {
        if let Some(warning) = static_fallback_warning(warning) {
            result.static_fallback_warnings.push(warning);
        } else {
            push_unique(
                &mut result.warning_unresolved_fields,
                "<static-warning-projection>",
            );
        }
    }
    result
}

/// Run Native `fTr` on an already source-selected managed-settings object.
/// This is also the runtime helper-output path: `Zi` validates the envelope
/// first, then calls `qn(managedSettings, "policyHelper")`, which is `fTr`
/// plus warning logging and intentionally does not run the static I5e gate.
pub(crate) fn validate_native_helper_managed_settings(
    raw: &Utf16JsonProjection,
    source_field: &str,
    semantics: &NativePolicySemanticEvidence,
) -> NativeStaticSettingsResult {
    // fTr's `wd(e)` is structuredClone with no transfer list. Cloning this
    // projection preserves all strings and exact property names.
    let mut at = raw.clone();
    let mut at_events = Vec::new();
    let mut at_warnings = Vec::new();
    let mut warning_unresolved_fields = Vec::new();
    let mut unresolved = Vec::new();
    let mut permission_result = None;
    let mut managed_mcp_diagnostics = Vec::new();
    let mut hooks_due_issues = Vec::new();
    let mut normalized_hooks = None;

    // fTr strips nested helper configuration before calling At.
    for field in ["policyHelper", "policyHelpers"] {
        if let Ok(value) = at.subprojection(&format!("/{field}")) {
            if !value.at_pointer("").is_some_and(JsonValueView::is_null) {
                at_events.push(NativeStaticAtEvent {
                    field: field.to_owned(),
                    kind: NativeStaticAtEventKind::AliasIgnored,
                    index: None,
                    raw_value: Some(value),
                });
            }
            let _ = at.remove_field(field);
        }
    }

    // At's exact order: Nue, f_, d_, u_, k_, v_, A_, R_, P_, m_, __, h_.
    normalize_marketplace_aliases(
        &mut at,
        &mut at_events,
        &mut at_warnings,
        &mut warning_unresolved_fields,
        source_field,
    );
    remove_restrictive_nulls(&mut at, &mut at_events);

    if let Ok(permissions) = at.subprojection("/permissions") {
        if permissions
            .at_pointer("")
            .is_some_and(|value| value.object_entries().is_some())
        {
            let result = normalize_native_permissions(
                &permissions,
                &NativePermissionSourceContext {
                    file: source_field.to_owned(),
                    policy_source: true,
                    strict_parse_follows: true,
                },
            );
            for unresolved_field in &result.unresolved_fields {
                push_unique(&mut unresolved, unresolved_field);
            }
            let _ = at.replace_pointer_projection("/permissions", result.normalized.clone());
            at_events.push(NativeStaticAtEvent {
                field: "permissions".to_owned(),
                kind: NativeStaticAtEventKind::PermissionResult,
                index: None,
                raw_value: Some(permissions),
            });
            permission_result = Some(result);
            append_permission_warnings(
                source_field,
                permission_result.as_ref().expect("result was just stored"),
                &mut at_warnings,
                &mut warning_unresolved_fields,
            );
        }
    }

    // u_ normalizes hook rows and emits the Native At warning projections.
    // Due.hooks is validated separately because it is a nested Zod record.
    let original_hooks = at.subprojection("/hooks").ok();
    let hook_result = normalize_native_hook_settings(
        &at,
        &NativeHookSourceContext {
            file: source_field.to_owned(),
        },
        &semantics.js,
    );
    at_warnings.extend(
        hook_result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.raw_warning.clone()),
    );
    if hook_result.unresolved {
        push_unique(&mut unresolved, "hooks");
        push_unique(&mut warning_unresolved_fields, "hooks");
    }
    match hook_result.normalized {
        Some(hooks) => {
            if at
                .replace_pointer_projection("/hooks", hooks.clone())
                .is_err()
            {
                push_unique(&mut unresolved, "hooks.<at-projection>");
            } else {
                let due_hooks = validate_native_hooks_due(&hooks, &semantics.js);
                if due_hooks.unresolved {
                    push_unique(&mut unresolved, "hooks.<due-projection>");
                }
                hooks_due_issues.extend(due_hooks.issues.into_iter().map(|issue| {
                    NativeStrictSettingsIssue {
                        path: issue.path,
                        message: issue.message,
                    }
                }));
                normalized_hooks = due_hooks.normalized;
            }
        }
        None if original_hooks.is_some() => {
            let _ = at.remove_field("hooks");
        }
        None => {}
    }

    // k_ validates/removes marketplace declarations and emits exact warning
    // rows. Due performs the separate final Go record projection below.
    let original_extra_marketplaces = at.subprojection("/extraKnownMarketplaces").ok();
    let extra_marketplaces =
        normalize_native_extra_known_marketplaces(&at, source_field, &semantics.js);
    at_warnings.extend(
        extra_marketplaces
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.raw_warning.clone()),
    );
    if extra_marketplaces.unresolved {
        push_unique(&mut unresolved, "extraKnownMarketplaces");
        push_unique(&mut warning_unresolved_fields, "extraKnownMarketplaces");
    }
    match extra_marketplaces.normalized {
        Some(value) => {
            if at
                .replace_pointer_projection("/extraKnownMarketplaces", value.clone())
                .is_err()
            {
                push_unique(&mut unresolved, "extraKnownMarketplaces.<at-projection>");
            }
            if original_extra_marketplaces
                .as_ref()
                .is_some_and(|original| !original.semantically_eq(&value))
            {
                at_events.push(NativeStaticAtEvent {
                    field: "extraKnownMarketplaces".to_owned(),
                    kind: NativeStaticAtEventKind::ExtraMarketplaceResultApplied,
                    index: None,
                    raw_value: original_extra_marketplaces,
                });
            }
        }
        None => {
            if let Some(original) = original_extra_marketplaces {
                let _ = at.remove_field("extraKnownMarketplaces");
                at_events.push(NativeStaticAtEvent {
                    field: "extraKnownMarketplaces".to_owned(),
                    kind: NativeStaticAtEventKind::ExtraMarketplaceFieldDropped,
                    index: None,
                    raw_value: Some(original),
                });
            }
        }
    }

    // v_ salvages valid modelPicker rows and removes invalid flags/rows before
    // the Due object strips unknown keys from the surviving row objects.
    transform_model_picker(&mut at, &mut at_events);

    // A_ independently drops invalid multipliers, invalid override roots, and
    // invalid/reserved override rows before Due sees the surviving map.
    transform_model_pricing(&mut at, &mut at_events, &mut unresolved);

    // R_ maps every non-enum value to "refuse" for policy sources.
    if let Ok(value) = at.subprojection("/crossSessionInbound") {
        if !value.at_pointer("").is_some_and(|root| {
            root.as_str()
                .is_some_and(|name| ["accept", "hold", "refuse"].contains(&name))
        }) {
            let replacement = Utf16JsonProjection::parse(r#""refuse""#)
                .expect("fixed Native enum value is valid JSON");
            let _ = at.replace_pointer_projection("/crossSessionInbound", replacement);
            at_events.push(NativeStaticAtEvent {
                field: "crossSessionInbound".to_owned(),
                kind: NativeStaticAtEventKind::CrossSessionInboundRestricted,
                index: None,
                raw_value: Some(value),
            });
        }
    }

    // P_ treats invalid remoteControl as the most restrictive value in a
    // policy source. Due later strips every unknown key from this object.
    transform_remote_control(&mut at, &mut at_events);

    // m_ has keepWholeFieldInvalid=true: wrong-shaped roots survive for Due,
    // while array entries are filtered by the exact current parser helper.
    for field in ["allowedMcpServers", "deniedMcpServers"] {
        transform_mcp_server_policy_list(&mut at, field, &mut at_events, &mut unresolved);
    }
    transform_managed_mcp_servers(
        &mut at,
        &mut at_events,
        &mut unresolved,
        &mut managed_mcp_diagnostics,
        &mut at_warnings,
        &mut warning_unresolved_fields,
        source_field,
        &semantics.js,
    );
    for field in ["strictKnownMarketplaces", "blockedMarketplaces"] {
        transform_marketplace_policy_list(
            &mut at,
            field,
            &mut unresolved,
            &mut warning_unresolved_fields,
            &semantics.regex_rows,
            &semantics.js,
        );
    }

    record_at_warning_gaps(&at_events, &mut warning_unresolved_fields);

    if let Some(rejection) = ftr_hard_rejection(&at) {
        return NativeStaticSettingsResult {
            raw: raw.clone(),
            after_at: Some(at),
            due_projection: None,
            normalized: None,
            at_events,
            at_warnings,
            static_fallback_warnings: Vec::new(),
            warning_unresolved_fields,
            permission_result,
            managed_mcp_diagnostics,
            regex_patterns: Vec::new(),
            i5e_issue: None,
            hard_rejection: Some(rejection),
            due_issues: Vec::new(),
            unresolved_fields: unresolved,
            status: NativeStaticSettingsStatus::Invalid,
        };
    }

    let regex_patterns = collect_native_regex_patterns(&at);
    let mut due_input = at.clone();
    if original_hooks.is_some() {
        let _ = due_input.remove_field("hooks");
    }
    // `At` has already produced the exact defaultMode diagnostic. Native's
    // Due parser catches that invalid enum to undefined, so do not feed the
    // same invalid value to the generic strict enum validator and duplicate
    // its issue.
    if permission_result
        .as_ref()
        .is_some_and(|result| result.default_mode.due_issue.is_some())
    {
        if let Ok(mut permissions) = due_input.subprojection("/permissions") {
            let _ = permissions.remove_field("defaultMode");
            let _ = due_input.replace_pointer_projection("/permissions", permissions);
        }
    }
    let mut normalized_extra_marketplaces = None;
    let mut extra_marketplace_due_issues = Vec::new();
    if let Ok(extra_marketplaces) = at.subprojection("/extraKnownMarketplaces") {
        let _ = due_input.remove_field("extraKnownMarketplaces");
        let due_extra =
            validate_native_extra_known_marketplaces_due(&extra_marketplaces, &semantics.js);
        if due_extra.unresolved {
            push_unique(&mut unresolved, "extraKnownMarketplaces.<due-projection>");
        }
        for issue in due_extra.issues {
            let Some(path) = dotted_native_issue_path(&issue.path) else {
                push_unique(&mut unresolved, "extraKnownMarketplaces.<due-issue-path>");
                continue;
            };
            extra_marketplace_due_issues.push(NativeStrictSettingsIssue {
                path,
                message: issue.message,
            });
        }
        normalized_extra_marketplaces = due_extra.normalized;
        if normalized_extra_marketplaces.is_none()
            && extra_marketplace_due_issues.is_empty()
            && !due_extra.unresolved
        {
            push_unique(&mut unresolved, "extraKnownMarketplaces.<due-result>");
        }
    }
    let due = validate_native_strict_settings(&due_input, &semantics.regex_rows, &semantics.js);
    let mut due_issues = due.issues;
    due_issues.extend(hooks_due_issues);
    due_issues.extend(extra_marketplace_due_issues);
    due_issues.sort_by_key(|issue| native_due_issue_order(&issue.path));
    due_issues.truncate(3);
    let mut due_projection = due.projection.clone();
    if let Some(hooks) = normalized_hooks {
        due_projection = insert_due_field_in_schema_order(due_projection, "hooks", hooks)
            .unwrap_or_else(|| {
                push_unique(&mut unresolved, "hooks.<due-output-projection>");
                due.projection.clone()
            });
    }
    if let Some(extra_marketplaces) = normalized_extra_marketplaces {
        due_projection = insert_due_field_in_schema_order(
            due_projection,
            "extraKnownMarketplaces",
            extra_marketplaces,
        )
        .unwrap_or_else(|| {
            push_unique(
                &mut unresolved,
                "extraKnownMarketplaces.<due-output-projection>",
            );
            due.projection.clone()
        });
    }
    let permission_arrays_valid = permission_result
        .as_ref()
        .is_some_and(NativePermissionResult::rule_arrays_due_valid);
    if let Some(issue) = permission_result
        .as_ref()
        .and_then(|result| result.default_mode.due_issue.as_ref())
    {
        if issue
            .message
            .at_pointer("")
            .and_then(JsonValueView::code_units)
            .is_none()
        {
            push_unique(
                &mut unresolved,
                "permissions.defaultMode.<issue-projection>",
            );
            return NativeStaticSettingsResult {
                raw: raw.clone(),
                after_at: Some(at),
                due_projection: Some(due_projection),
                normalized: None,
                at_events,
                at_warnings,
                static_fallback_warnings: Vec::new(),
                warning_unresolved_fields,
                permission_result,
                managed_mcp_diagnostics,
                regex_patterns,
                i5e_issue: None,
                hard_rejection: None,
                due_issues,
                unresolved_fields: unresolved,
                status: NativeStaticSettingsStatus::Unresolved,
            };
        }
        due_issues.push(NativeStrictSettingsIssue {
            path: string_projection(issue.path),
            message: issue.message.clone(),
        });
        due_issues.sort_by_key(|issue| native_due_issue_order(&issue.path));
        due_issues.truncate(3);
    }
    for field in &due.unresolved_fields {
        if permission_arrays_valid
            && matches!(
                field.as_str(),
                "permissions.allow" | "permissions.deny" | "permissions.ask"
            )
        {
            continue;
        }
        if permission_result.is_some() && field == "permissions.defaultMode" {
            continue;
        }
        push_unique(&mut unresolved, field);
    }
    let status = if !due_issues.is_empty() {
        NativeStaticSettingsStatus::Invalid
    } else if !unresolved.is_empty() {
        NativeStaticSettingsStatus::Unresolved
    } else {
        NativeStaticSettingsStatus::Valid
    };
    let normalized = (status == NativeStaticSettingsStatus::Valid)
        .then(|| apply_allowed_providers_o_transform(&at, due_projection.clone()));

    NativeStaticSettingsResult {
        raw: raw.clone(),
        after_at: Some(at),
        due_projection: Some(due_projection),
        normalized,
        at_events,
        at_warnings,
        static_fallback_warnings: Vec::new(),
        warning_unresolved_fields,
        permission_result,
        managed_mcp_diagnostics,
        regex_patterns,
        i5e_issue: None,
        hard_rejection: None,
        due_issues,
        unresolved_fields: unresolved,
        status,
    }
}

fn invalid_result(
    raw: &Utf16JsonProjection,
    issue: Option<NativeStrictSettingsIssue>,
) -> NativeStaticSettingsResult {
    NativeStaticSettingsResult {
        raw: raw.clone(),
        after_at: None,
        due_projection: None,
        normalized: None,
        at_events: Vec::new(),
        at_warnings: Vec::new(),
        static_fallback_warnings: Vec::new(),
        warning_unresolved_fields: Vec::new(),
        permission_result: None,
        managed_mcp_diagnostics: Vec::new(),
        regex_patterns: Vec::new(),
        i5e_issue: issue,
        hard_rejection: None,
        due_issues: Vec::new(),
        unresolved_fields: Vec::new(),
        status: NativeStaticSettingsStatus::Invalid,
    }
}

fn insert_due_field_in_schema_order(
    projection: Utf16JsonProjection,
    field: &str,
    value: Utf16JsonProjection,
) -> Option<Utf16JsonProjection> {
    let entries = projection.at_pointer("")?.object_entries()?;
    let field_units = field.encode_utf16().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(entries.len() + 1);
    let mut inserted = false;
    for schema_field in NATIVE_DUE_FIELD_ORDER {
        let schema_units = schema_field.encode_utf16().collect::<Vec<_>>();
        if schema_units == field_units {
            output.push((field_units.clone(), value.clone()));
            inserted = true;
            continue;
        }
        if let Some(entry) = entries
            .iter()
            .find(|entry| entry.code_units == schema_units)
        {
            output.push((
                entry.code_units.to_vec(),
                projection
                    .subprojection_property_units(entry.code_units)
                    .ok()?,
            ));
        }
    }
    for entry in entries {
        let known = NATIVE_DUE_FIELD_ORDER.iter().any(|schema_field| {
            schema_field
                .encode_utf16()
                .eq(entry.code_units.iter().copied())
        });
        if !known {
            output.push((
                entry.code_units.to_vec(),
                projection
                    .subprojection_property_units(entry.code_units)
                    .ok()?,
            ));
        }
    }
    if !inserted {
        return None;
    }
    Utf16JsonProjection::object_units(output).ok()
}

fn native_due_issue_order(path: &Utf16JsonProjection) -> (usize, usize) {
    let Some(path_units) = path.at_pointer("").and_then(JsonValueView::code_units) else {
        return (NATIVE_DUE_FIELD_ORDER.len(), usize::MAX);
    };
    let mut components = path_units.split(|unit| *unit == u16::from(b'.'));
    let field = components.next().unwrap_or_default();
    let top_level = NATIVE_DUE_FIELD_ORDER
        .iter()
        .position(|candidate| candidate.encode_utf16().eq(field.iter().copied()))
        .unwrap_or(NATIVE_DUE_FIELD_ORDER.len());
    let permission_field = "permissions".encode_utf16().collect::<Vec<_>>();
    let nested = if field == permission_field.as_slice() {
        let child = components.next().unwrap_or_default();
        [
            "allow",
            "deny",
            "ask",
            "defaultMode",
            "disableBypassPermissionsMode",
            "disableAutoMode",
            "blockReadsOutsideWorkingDirectories",
            "additionalDirectories",
        ]
        .iter()
        .position(|candidate| candidate.encode_utf16().eq(child.iter().copied()))
        .unwrap_or(8)
    } else {
        0
    };
    (top_level, nested)
}

fn normalize_marketplace_aliases(
    projection: &mut Utf16JsonProjection,
    events: &mut Vec<NativeStaticAtEvent>,
    warnings: &mut Vec<Utf16JsonProjection>,
    warning_unresolved: &mut Vec<String>,
    file: &str,
) {
    // Exact O5e order. fTr does not pass At's loneNullAlias:"rename" option.
    for (alias, canonical) in [
        ("additionalMarketplaces", "extraKnownMarketplaces"),
        ("allowedMarketplaces", "strictKnownMarketplaces"),
    ] {
        let Ok(alias_value) = projection.subprojection(&format!("/{alias}")) else {
            continue;
        };
        if alias_value
            .at_pointer("")
            .is_some_and(JsonValueView::is_null)
        {
            let _ = projection.remove_field(alias);
            events.push(NativeStaticAtEvent {
                field: alias.to_owned(),
                kind: NativeStaticAtEventKind::AliasNullRemoved,
                index: None,
                raw_value: Some(alias_value),
            });
            continue;
        }
        let canonical_value = projection.subprojection(&format!("/{canonical}")).ok();
        let canonical_non_null = canonical_value
            .as_ref()
            .is_some_and(|value| !value.at_pointer("").is_some_and(JsonValueView::is_null));
        if canonical_non_null {
            let message = format!(
                "\"{alias}\" is an alias for \"{canonical}\" and this file sets both; the \"{alias}\" value was ignored. Use only \"{canonical}\"."
            );
            let extras = vec![
                ("alias".encode_utf16().collect(), string_projection(alias)),
                (
                    "canonical".encode_utf16().collect(),
                    string_projection(canonical),
                ),
            ];
            if let Some(warning) = native_warning_row(
                file,
                &alias.encode_utf16().collect::<Vec<_>>(),
                &message.encode_utf16().collect::<Vec<_>>(),
                extras,
            ) {
                warnings.push(warning);
            } else {
                push_unique(warning_unresolved, alias);
            }
            events.push(NativeStaticAtEvent {
                field: alias.to_owned(),
                kind: NativeStaticAtEventKind::AliasIgnored,
                index: None,
                raw_value: Some(alias_value),
            });
        } else {
            let _ = projection.set_field(canonical, alias_value.clone());
        }
        let _ = projection.remove_field(alias);
    }
}

/// Top-level projection of `_n()`: the two marketplace restrictions, strict
/// plugin customization, and Qo's one-segment boolean/string restrictions.
/// `disableAllHooks` is explicitly excluded by Qo and must not be included.
const NATIVE_NULL_REMOVAL_FIELDS: &[&str] = &[
    "strictKnownMarketplaces",
    "blockedMarketplaces",
    "strictPluginOnlyCustomization",
    "allowManagedPermissionRulesOnly",
    "allowManagedHooksOnly",
    "allowManagedMcpServersOnly",
    "enforceAvailableModels",
    "disableClaudeAiConnectors",
    "disableCommandPluginSources",
    "disableSideloadFlags",
    "disableSkillShellExecution",
    "disableRemoteControl",
    "disableAgentView",
    "disableWorkflows",
    "disableArtifact",
    "disableBundledSkills",
    "fastModePerSessionOptIn",
    "isolatePeerMachines",
    "disableAutoMode",
    "disableDeepLinkRegistration",
    "enableArtifact",
    "enableWorkflows",
    "syncClaudeAiSkills",
    "syncClaudeAiPlugins",
    "useAutoModeDuringPlan",
    "skipDangerousModePermissionPrompt",
    "skipAutoPermissionPrompt",
    "enableAllProjectMcpServers",
    "channelsEnabled",
    "skipWebFetchPreflight",
    "skipWorkflowUsageWarning",
    "autoUploadSessions",
    "remoteControlAtStartup",
    "autoContinueAtUsageLimit",
    "disablePluginDirectory",
    "feedbackDrafts",
    "availableModelsMatch",
    "askUserQuestionTimeout",
    "dialogExpiry",
];

fn remove_restrictive_nulls(
    projection: &mut Utf16JsonProjection,
    events: &mut Vec<NativeStaticAtEvent>,
) {
    for field in NATIVE_NULL_REMOVAL_FIELDS {
        let Ok(value) = projection.subprojection(&format!("/{field}")) else {
            continue;
        };
        if value.at_pointer("").is_some_and(JsonValueView::is_null) {
            let _ = projection.remove_field(field);
            events.push(NativeStaticAtEvent {
                field: (*field).to_owned(),
                kind: NativeStaticAtEventKind::RestrictiveNullRemoved,
                index: None,
                raw_value: Some(value),
            });
        }
    }
}

fn transform_mcp_server_policy_list(
    projection: &mut Utf16JsonProjection,
    field: &str,
    events: &mut Vec<NativeStaticAtEvent>,
    unresolved: &mut Vec<String>,
) {
    let Ok(value) = projection.subprojection(&format!("/{field}")) else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        push_unique(unresolved, field);
        return;
    };
    let Some(length) = root.array_len() else {
        // fTr enables keepWholeFieldInvalid, so Due sees and rejects this root.
        return;
    };
    let allowlist = field == "allowedMcpServers";
    let mut retained = Vec::new();
    for index in 0..length {
        let Ok(item) = value.subprojection(&format!("/{index}")) else {
            push_unique(unresolved, field);
            continue;
        };
        if let Some(normalized) = normalize_mcp_server_entry(&item, allowlist) {
            retained.push(normalized);
        } else {
            events.push(NativeStaticAtEvent {
                field: field.to_owned(),
                kind: NativeStaticAtEventKind::McpServerEntryDropped,
                index: Some(index),
                raw_value: Some(item),
            });
        }
    }
    let normalized =
        Utf16JsonProjection::array(retained).expect("filtered Native MCP entries form an array");
    let _ = projection.replace_pointer_projection(&format!("/{field}"), normalized);
}

fn transform_marketplace_policy_list(
    projection: &mut Utf16JsonProjection,
    field: &str,
    unresolved: &mut Vec<String>,
    warning_unresolved: &mut Vec<String>,
    regex_rows: &[NativePolicyRegexValidationRow],
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let Ok(value) = projection.subprojection(&format!("/{field}")) else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        push_unique(unresolved, field);
        return;
    };
    if root.array_len().is_none() {
        // Native h_ only visits arrays; Due handles other roots.
        return;
    }
    let result = normalize_policy_marketplace_field(field, &value, regex_rows, Some(semantics));
    if !result.diagnostics.is_empty() {
        push_unique(warning_unresolved, field);
    }
    if result.unresolved {
        push_unique(unresolved, field);
        return;
    }
    match result.normalized {
        Some(normalized) => {
            let _ = projection.replace_pointer_projection(&format!("/{field}"), normalized);
        }
        None => {
            let _ = projection.remove_field(field);
        }
    }
}

fn transform_model_pricing(
    projection: &mut Utf16JsonProjection,
    events: &mut Vec<NativeStaticAtEvent>,
    unresolved: &mut Vec<String>,
) {
    let Ok(value) = projection.subprojection("/modelPricing") else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        push_unique(unresolved, "modelPricing");
        return;
    };
    if js_falsy(root) || root.object_entries().is_none() {
        let _ = projection.remove_field("modelPricing");
        events.push(NativeStaticAtEvent {
            field: "modelPricing".to_owned(),
            kind: NativeStaticAtEventKind::ModelPricingRootDropped,
            index: None,
            raw_value: Some(value),
        });
        return;
    }

    let mut output = Vec::new();
    for entry in root.object_entries().unwrap_or_default() {
        let Ok(child) = value.subprojection_property_units(entry.code_units) else {
            push_unique(unresolved, "modelPricing.<unrepresentable-key>");
            continue;
        };
        if entry.code_units == "multiplier".encode_utf16().collect::<Vec<_>>() {
            if !native_model_pricing_multiplier_valid(&child) {
                events.push(NativeStaticAtEvent {
                    field: "modelPricing.multiplier".to_owned(),
                    kind: NativeStaticAtEventKind::ModelPricingMultiplierDropped,
                    index: None,
                    raw_value: Some(child),
                });
                continue;
            }
            output.push((entry.code_units.to_vec(), child));
            continue;
        }
        if entry.code_units == "overrides".encode_utf16().collect::<Vec<_>>() {
            let Some(overrides_root) = child.at_pointer("") else {
                push_unique(unresolved, "modelPricing.overrides");
                continue;
            };
            if js_falsy(overrides_root) || overrides_root.object_entries().is_none() {
                events.push(NativeStaticAtEvent {
                    field: "modelPricing.overrides".to_owned(),
                    kind: NativeStaticAtEventKind::ModelPricingOverridesDropped,
                    index: None,
                    raw_value: Some(child),
                });
                continue;
            }
            let mut rows = Vec::new();
            for row_entry in overrides_root.object_entries().unwrap_or_default() {
                let Ok(row) = child.subprojection_property_units(row_entry.code_units) else {
                    push_unique(unresolved, "modelPricing.overrides.<unrepresentable-key>");
                    continue;
                };
                let key = String::from_utf16(row_entry.code_units)
                    .unwrap_or_else(|_| "<unrepresentable-key>".to_owned());
                match normalize_model_pricing_override(&row) {
                    Some(normalized) if key != "constructor" && key != "__proto__" => {
                        rows.push((row_entry.code_units.to_vec(), normalized));
                    }
                    _ => events.push(NativeStaticAtEvent {
                        field: format!("modelPricing.overrides.{key}"),
                        kind: NativeStaticAtEventKind::ModelPricingOverrideDropped,
                        index: None,
                        raw_value: Some(row),
                    }),
                }
            }
            match Utf16JsonProjection::object_units(rows) {
                Ok(normalized) => output.push((entry.code_units.to_vec(), normalized)),
                Err(_) => push_unique(unresolved, "modelPricing.overrides"),
            }
            continue;
        }
        // Native At leaves other keys in place; Due's object schema strips them.
        output.push((entry.code_units.to_vec(), child));
    }
    if let Ok(normalized) = Utf16JsonProjection::object_units(output) {
        let _ = projection.replace_pointer_projection("/modelPricing", normalized);
    } else {
        push_unique(unresolved, "modelPricing");
    }
}

fn native_model_pricing_multiplier_valid(value: &Utf16JsonProjection) -> bool {
    value
        .at_pointer("")
        .and_then(JsonValueView::as_number)
        .is_some_and(|number| number.is_finite() && number > 0.0 && number <= 10.0)
}

fn normalize_model_pricing_override(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let root = value.at_pointer("")?;
    let entries = root.object_entries()?;
    let mut fields = Vec::with_capacity(4);
    for name in ["input", "output", "cacheRead", "cacheWrite"] {
        let field = root.get_property(name)?;
        let number = field.as_number()?;
        if !number.is_finite() || !(0.0..=10_000.0).contains(&number) {
            return None;
        }
        fields.push((
            name.to_owned(),
            value.subprojection(&format!("/{name}")).ok()?,
        ));
    }
    let _ = entries; // Required fields were read above; unknown keys are stripped.
    Utf16JsonProjection::object(fields).ok()
}

fn transform_managed_mcp_servers(
    projection: &mut Utf16JsonProjection,
    events: &mut Vec<NativeStaticAtEvent>,
    unresolved: &mut Vec<String>,
    diagnostics: &mut Vec<NativeManagedMcpServerDiagnostic>,
    warnings: &mut Vec<Utf16JsonProjection>,
    warning_unresolved: &mut Vec<String>,
    source_field: &str,
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let Ok(value) = projection.subprojection("/managedMcpServers") else {
        return;
    };
    let result = normalize_native_managed_mcp_servers(&value, semantics);
    if result.unresolved {
        push_unique(unresolved, "managedMcpServers");
    }
    for diagnostic in &result.diagnostics {
        if let Some(warning) = managed_mcp_warning(source_field, diagnostic) {
            warnings.push(warning);
        } else {
            push_unique(warning_unresolved, "managedMcpServers");
        }
        events.push(NativeStaticAtEvent {
            field: "managedMcpServers".to_owned(),
            kind: NativeStaticAtEventKind::ManagedMcpServerDiagnostic,
            index: None,
            raw_value: diagnostic.raw_value.clone(),
        });
    }
    diagnostics.extend(result.diagnostics);
    match result.normalized {
        Some(normalized) => {
            let _ = projection.replace_pointer_projection("/managedMcpServers", normalized);
        }
        None => {
            let _ = projection.remove_field("managedMcpServers");
        }
    }
}

fn transform_model_picker(
    projection: &mut Utf16JsonProjection,
    events: &mut Vec<NativeStaticAtEvent>,
) {
    let Ok(value) = projection.subprojection("/modelPicker") else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.object_entries().is_none()
        || root
            .get_property("options")
            .and_then(JsonValueView::array_len)
            .is_none()
    {
        let _ = projection.remove_field("modelPicker");
        events.push(NativeStaticAtEvent {
            field: "modelPicker".to_owned(),
            kind: NativeStaticAtEventKind::ModelPickerDropped,
            index: None,
            raw_value: Some(value),
        });
        return;
    }

    let mut options = Vec::new();
    let count = value.array_len("/options").unwrap_or(0);
    for index in 0..count {
        let Ok(row) = value.subprojection(&format!("/options/{index}")) else {
            continue;
        };
        if model_picker_row_is_valid(&row) {
            options.push(row);
        } else {
            events.push(NativeStaticAtEvent {
                field: "modelPicker.options".to_owned(),
                kind: NativeStaticAtEventKind::ModelPickerOptionDropped,
                index: Some(index),
                raw_value: Some(row),
            });
        }
    }
    let options = Utf16JsonProjection::array(options).expect("modelPicker rows are JSON values");
    let entries = value.object_entries().unwrap_or_default();
    let mut output = Vec::with_capacity(entries.len());
    for entry in entries {
        let key = entry.code_units.to_vec();
        if key == "options".encode_utf16().collect::<Vec<_>>() {
            output.push((key, options.clone()));
        } else if key == "replaceBuiltInOptions".encode_utf16().collect::<Vec<_>>() {
            if entry.value.as_bool().is_some() {
                output.push((
                    key,
                    value
                        .subprojection_property_units(entry.code_units)
                        .expect("entry exists"),
                ));
            } else {
                events.push(NativeStaticAtEvent {
                    field: "modelPicker.replaceBuiltInOptions".to_owned(),
                    kind: NativeStaticAtEventKind::ModelPickerReplaceFlagDropped,
                    index: None,
                    raw_value: value.subprojection_property_units(entry.code_units).ok(),
                });
            }
        } else {
            output.push((
                key,
                value
                    .subprojection_property_units(entry.code_units)
                    .expect("entry exists"),
            ));
        }
    }
    if let Ok(normalized) = Utf16JsonProjection::object_units(output) {
        let _ = projection.replace_pointer_projection("/modelPicker", normalized);
    }
}

fn model_picker_row_is_valid(row: &Utf16JsonProjection) -> bool {
    let Some(root) = row.at_pointer("") else {
        return false;
    };
    if root.object_entries().is_none()
        || root
            .get_property("model")
            .and_then(JsonValueView::code_units)
            .is_none()
    {
        return false;
    }
    ["label", "description", "behavesAs"].iter().all(|field| {
        root.get_property(field)
            .is_none_or(|value| value.code_units().is_some())
    })
}

fn transform_remote_control(
    projection: &mut Utf16JsonProjection,
    events: &mut Vec<NativeStaticAtEvent>,
) {
    let Ok(value) = projection.subprojection("/remoteControl") else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if !is_plain_object(&value) {
        let replacement = Utf16JsonProjection::parse(r#"{"shareHostProfile":"off"}"#)
            .expect("fixed Native restrictive projection is valid JSON");
        let _ = projection.replace_pointer_projection("/remoteControl", replacement);
        events.push(NativeStaticAtEvent {
            field: "remoteControl".to_owned(),
            kind: NativeStaticAtEventKind::RemoteControlRestricted,
            index: None,
            raw_value: Some(value),
        });
        return;
    }

    let has_unknown = root.object_entries().is_some_and(|entries| {
        entries
            .iter()
            .any(|entry| entry.code_units != "shareHostProfile".encode_utf16().collect::<Vec<_>>())
    });
    let profile = root.get_property("shareHostProfile");
    let profile_valid = profile.is_none_or(|value| {
        value
            .as_str()
            .is_some_and(|name| ["off", "basic", "full"].contains(&name))
    });
    if has_unknown || !profile_valid {
        let mut normalized = value.clone();
        let _ = normalized.set_field(
            "shareHostProfile",
            Utf16JsonProjection::parse(r#""off""#).expect("fixed enum value is valid JSON"),
        );
        let _ = projection.replace_pointer_projection("/remoteControl", normalized);
        events.push(NativeStaticAtEvent {
            field: "remoteControl.shareHostProfile".to_owned(),
            kind: NativeStaticAtEventKind::RemoteControlRestricted,
            index: None,
            raw_value: Some(value),
        });
    }
}

fn ftr_hard_rejection(projection: &Utf16JsonProjection) -> Option<NativeStaticHardRejection> {
    // hTr runs before yTr and Due.
    if let Ok(value) = projection.subprojection("/strictPluginOnlyCustomization") {
        if let Some(root) = value.at_pointer("") {
            if !root.is_null() && root.as_bool().is_none() && root.array_len().is_none() {
                return Some(NativeStaticHardRejection::StrictPluginOnlyCustomization {
                    raw_value: value,
                });
            }
        }
    }

    if let Ok(value) = projection.subprojection("/allowedProviders") {
        let Some(root) = value.at_pointer("") else {
            return Some(NativeStaticHardRejection::AllowedProvidersNotArray { raw_value: value });
        };
        let Some(length) = root.array_len() else {
            return Some(NativeStaticHardRejection::AllowedProvidersNotArray { raw_value: value });
        };
        for index in 0..length {
            let item = root.get_index(index).expect("array index exists");
            if item.code_units().is_none() {
                return Some(NativeStaticHardRejection::AllowedProvidersNotStrings {
                    raw_value: value,
                    index,
                });
            }
        }
        let rejected_names = (0..length)
            .filter_map(|index| {
                let item = value.subprojection(&format!("/{index}")).ok()?;
                let name = item.at_pointer("")?.code_units()?;
                (!is_native_provider_name(name)).then_some(item)
            })
            .collect::<Vec<_>>();
        if !rejected_names.is_empty() {
            return Some(NativeStaticHardRejection::AllowedProvidersUnknownName {
                raw_value: value,
                rejected_names,
            });
        }
    }
    None
}

fn apply_allowed_providers_o_transform(
    source: &Utf16JsonProjection,
    due: Utf16JsonProjection,
) -> Utf16JsonProjection {
    let Ok(value) = source.subprojection("/allowedProviders") else {
        return due;
    };
    let Some(root) = value.at_pointer("") else {
        return due;
    };
    let Some(length) = root.array_len() else {
        return due;
    };
    let kept = (0..length)
        .filter_map(|index| value.subprojection(&format!("/{index}")).ok())
        .filter(|item| {
            item.at_pointer("")
                .and_then(JsonValueView::code_units)
                .is_some_and(is_native_provider_name)
        })
        .collect::<Vec<_>>();
    let array = Utf16JsonProjection::array(kept).expect("allowed provider strings form an array");
    assign_due_field_like_object_assign(due, "allowedProviders", array)
        .expect("Native Due returns an object projection")
}

/// Native `O_` applies `Object.assign(due, { allowedProviders })` after Due
/// parsing. Existing own properties keep their insertion slot; absent ones
/// append after all properties already present.
fn assign_due_field_like_object_assign(
    projection: Utf16JsonProjection,
    field: &str,
    value: Utf16JsonProjection,
) -> Option<Utf16JsonProjection> {
    let entries = projection.at_pointer("")?.object_entries()?;
    let field_units = field.encode_utf16().collect::<Vec<_>>();
    let mut output = Vec::with_capacity(entries.len() + 1);
    let mut replaced = false;
    for entry in entries {
        let current = if entry.code_units == field_units {
            replaced = true;
            value.clone()
        } else {
            projection
                .subprojection_property_units(entry.code_units)
                .ok()?
        };
        output.push((entry.code_units.to_vec(), current));
    }
    if !replaced {
        output.push((field_units, value));
    }
    Utf16JsonProjection::object_units(output).ok()
}

fn is_native_provider_name(units: &[u16]) -> bool {
    if NATIVE_PROVIDER_NAMES
        .iter()
        .any(|name| units == name.encode_utf16().collect::<Vec<_>>())
    {
        return true;
    }
    // Exact `^anthropic[A-Z][A-Za-z0-9]*$` extension accepted by Pt/yTr.
    let prefix = "anthropic".encode_utf16().collect::<Vec<_>>();
    if units.len() < prefix.len() + 1 || !units.starts_with(&prefix) {
        return false;
    }
    let first = units[prefix.len()];
    if !(u16::from(b'A')..=u16::from(b'Z')).contains(&first) {
        return false;
    }
    units[prefix.len() + 1..].iter().all(|unit| {
        (u16::from(b'A')..=u16::from(b'Z')).contains(unit)
            || (u16::from(b'a')..=u16::from(b'z')).contains(unit)
            || (u16::from(b'0')..=u16::from(b'9')).contains(unit)
    })
}

fn is_plain_object(value: &Utf16JsonProjection) -> bool {
    value
        .at_pointer("")
        .is_some_and(|root| root.object_entries().is_some())
}

fn js_falsy(value: JsonValueView<'_>) -> bool {
    value.is_null()
        || value.as_bool() == Some(false)
        || value
            .as_number()
            .is_some_and(|number| number == 0.0 || number.is_nan())
        || value.code_units().is_some_and(<[u16]>::is_empty)
}

fn push_unique(fields: &mut Vec<String>, value: &str) {
    if !fields.iter().any(|field| field == value) {
        fields.push(value.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_allowed_providers_o_transform, assign_due_field_like_object_assign,
        native_qn_log_messages, native_static_settings_diagnostic,
        validate_native_static_default_settings as validate_native_static_default_settings_source,
        NativePolicyRegexValidationRow, NativeStaticHardRejection,
        NativeStaticSettingsDiagnosticKind, NativeStaticSettingsStatus,
    };
    use crate::settings::native_policy::NativePolicySemanticEvidence;
    use crate::settings::native_policy_js_semantics::NativePolicyJsSemanticsRegistry;
    use crate::settings::native_policy_regex::NativePolicyRegexValidationOutcome;
    use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("fixture is valid JSON")
    }

    fn validate_native_static_default_settings(
        raw: &Utf16JsonProjection,
        source_field: &str,
        regex_rows: &[NativePolicyRegexValidationRow],
    ) -> super::NativeStaticSettingsResult {
        let semantics = NativePolicySemanticEvidence {
            regex_rows: regex_rows.to_vec(),
            js: NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new()).unwrap(),
        };
        validate_native_static_default_settings_source(raw, source_field, &semantics)
    }

    fn validate_native_helper_managed_settings(
        raw: &Utf16JsonProjection,
        source_field: &str,
        regex_rows: &[NativePolicyRegexValidationRow],
    ) -> super::NativeStaticSettingsResult {
        let semantics = NativePolicySemanticEvidence {
            regex_rows: regex_rows.to_vec(),
            js: NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new()).unwrap(),
        };
        super::validate_native_helper_managed_settings(raw, source_field, &semantics)
    }

    fn validate(source: &str) -> super::NativeStaticSettingsResult {
        validate_native_static_default_settings(&parse(source), "policyHelpers.default", &[])
    }

    fn validate_with_regex(
        source: &str,
        rows: &[NativePolicyRegexValidationRow],
    ) -> super::NativeStaticSettingsResult {
        validate_native_static_default_settings(&parse(source), "policyHelpers.default", rows)
    }

    #[test]
    fn regex_preflight_collects_only_reachable_mask_patterns_and_reuses_rows() {
        let source = r#"{"sandbox":{"credentials":{"files":[{"path":"/tmp/key","mode":"mask","extract":"secret=(.*)"},{"path":"/tmp/deny","mode":"deny","extract":"ignored["}],"envVars":[{"name":"TOKEN","mode":"mask","extract":"secret=(.*)"}]}}}"#;
        let preflight = validate(source);
        assert_eq!(
            preflight.regex_patterns,
            vec!["secret=(.*)".encode_utf16().collect::<Vec<_>>()]
        );
        assert!(preflight
            .unresolved_fields
            .iter()
            .any(|path| path.starts_with("sandbox.credentials.files.0.extract")));

        let pattern = "secret=(.*)".encode_utf16().collect::<Vec<_>>();
        let validated = validate_with_regex(
            source,
            &[NativePolicyRegexValidationRow {
                pattern_units: pattern.clone(),
                outcome: NativePolicyRegexValidationOutcome::Valid {
                    source_units: pattern,
                    capture_count: 1,
                },
            }],
        );
        assert_eq!(validated.status, NativeStaticSettingsStatus::Valid);
        assert!(validated.unresolved_fields.is_empty());
    }

    #[test]
    fn empty_and_model_only_payloads_follow_static_chain() {
        for source in [r#"{}"#, r#"{"model":"claude-sonnet-4-6"}"#] {
            let result = validate(source);
            assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
            assert_eq!(result.raw.to_json_string().unwrap(), source);
            assert!(result.due_issues.is_empty());
            assert!(result.unresolved_fields.is_empty());
            assert!(result.normalized.is_some());
        }
    }

    #[test]
    fn aliases_and_restrictive_nulls_are_applied_before_due() {
        let result = validate(
            r#"{"additionalMarketplaces":null,"allowedMarketplaces":null,"strictKnownMarketplaces":null,"blockedMarketplaces":null,"model":"x"}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        let normalized = result.normalized.unwrap();
        for field in [
            "additionalMarketplaces",
            "allowedMarketplaces",
            "strictKnownMarketplaces",
            "blockedMarketplaces",
        ] {
            assert!(normalized.subprojection(&format!("/{field}")).is_err());
        }
        assert_eq!(normalized.string_at("/model"), Some("x"));
    }

    #[test]
    fn empty_permission_buckets_use_the_shared_native_normalizer() {
        let result = validate(
            r#"{"model":"fallback","permissions":{"allow":[],"deny":[],"ask":[],"additionalDirectories":["/repo"]}}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        assert_eq!(
            result
                .normalized
                .as_ref()
                .unwrap()
                .array_len("/permissions/allow"),
            Some(0)
        );
        assert_eq!(
            result
                .normalized
                .as_ref()
                .unwrap()
                .string_at("/permissions/additionalDirectories/0"),
            Some("/repo")
        );
        assert!(result.permission_result.is_some());
    }

    #[test]
    fn permission_rule_arrays_are_validated_by_the_shared_source_rule_engine() {
        let result = validate_native_static_default_settings(
            &parse(
                r#"{"permissions":{"allow":["Read"],"deny":[],"ask":[],"defaultMode":"manual"}}"#,
            ),
            "policyHelpers.default",
            &[],
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        assert_eq!(
            result
                .normalized
                .as_ref()
                .unwrap()
                .string_at("/permissions/defaultMode"),
            Some("default")
        );
        assert_eq!(
            result
                .normalized
                .as_ref()
                .unwrap()
                .array_len("/permissions/allow"),
            Some(1)
        );
        assert!(result.due_issues.is_empty());
        assert!(result.unresolved_fields.is_empty());
    }

    #[test]
    fn invalid_permission_default_mode_uses_exact_source_zod_issue() {
        for source in [
            r#"{"permissions":{"defaultMode":"not-a-mode"}}"#,
            r#"{"permissions":{"defaultMode":null}}"#,
            r#"{"permissions":{"defaultMode":7}}"#,
        ] {
            let result = validate(source);
            assert_eq!(result.status, NativeStaticSettingsStatus::Invalid);
            assert_eq!(result.due_issues.len(), 1);
            assert_eq!(
                result.due_issues[0].path.string_at(""),
                Some("permissions.defaultMode")
            );
            assert_eq!(
                result.due_issues[0].message.string_at(""),
                Some("Invalid option: expected one of \"acceptEdits\"|\"auto\"|\"bypassPermissions\"|\"default\"|\"dontAsk\"|\"plan\"")
            );
            assert!(result.normalized.is_none());
        }
    }

    #[test]
    fn static_tr_strips_nested_helpers_before_i5e_but_rejects_entry_fields() {
        let nested = validate(r#"{"policyHelper":{"path":"/tmp/ignored"}}"#);
        assert_eq!(nested.status, NativeStaticSettingsStatus::Valid);
        assert!(nested
            .normalized
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelper")
            .is_none());
        assert_eq!(
            nested.raw.to_json_string().unwrap(),
            r#"{"policyHelper":{"path":"/tmp/ignored"}}"#
        );

        let result = validate(r#"{"path":"/tmp/ignored"}"#);
        assert_eq!(result.status, NativeStaticSettingsStatus::Invalid);
        let issue = result.i5e_issue.expect("I5e reports the forbidden field");
        assert_eq!(issue.path.string_at(""), Some("policyHelpers.default"));
        assert_eq!(
            issue.message.string_at(""),
            Some("must not contain \"path\" — a static payload is a managed-settings object, not a policyHelpers entry; entry fields (path/script/interpreter/outputBehavior/onFailure/retries/timeoutMs/refreshIntervalMs/defaultSettings) belong on the per-OS entries (policyHelpers.macos/linux/windows/wsl)")
        );
        assert_eq!(
            result.raw.to_json_string().unwrap(),
            r#"{"path":"/tmp/ignored"}"#
        );
    }

    #[test]
    fn dynamic_helper_managed_settings_uses_ftr_without_i5e() {
        let raw = parse(r#"{"path":"/tmp/ordinary-passthrough-key"}"#);
        let result = validate_native_helper_managed_settings(&raw, "policyHelper", &[]);
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        assert!(result.i5e_issue.is_none());
        assert_eq!(
            result
                .normalized
                .as_ref()
                .and_then(|value| value.string_at("/path")),
            Some("/tmp/ordinary-passthrough-key")
        );

        let invalid = validate_native_helper_managed_settings(
            &parse(r#"{"defaultShell":"invalid"}"#),
            "policyHelper",
            &[],
        );
        let error = super::native_ftr_settings_error(&invalid, "policyHelper")
            .expect("invalid settings produce a Native diagnostic");
        assert_eq!(
            error.string_at(""),
            Some("managedSettings rejected: defaultShell: Invalid option: expected one of \"bash\"|\"powershell\"")
        );
    }

    #[test]
    fn static_tr_skips_i5e_for_array_roots_and_lets_ftr_reject_due() {
        let result = validate("[]");
        assert_eq!(result.status, NativeStaticSettingsStatus::Invalid);
        assert!(result.i5e_issue.is_none());
        assert_eq!(result.due_issues.len(), 1);
        assert_eq!(result.due_issues[0].path.string_at(""), Some(""));
    }

    #[test]
    fn model_picker_salvages_rows_then_due_strips_unknown_row_keys() {
        let result = validate(
            r#"{"modelPicker":{"options":[{"model":"opus","label":"Primary","extra":true},{"model":4},{"model":"sonnet","behavesAs":"known","extra":false}],"replaceBuiltInOptions":"bad","extra":true}}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        let normalized = result.normalized.unwrap();
        assert_eq!(normalized.array_len("/modelPicker/options"), Some(2));
        assert_eq!(
            normalized.string_at("/modelPicker/options/0/model"),
            Some("opus")
        );
        assert!(normalized
            .subprojection("/modelPicker/options/0/extra")
            .is_err());
        assert!(normalized
            .subprojection("/modelPicker/replaceBuiltInOptions")
            .is_err());
        assert!(normalized.subprojection("/modelPicker/extra").is_err());
    }

    #[test]
    fn model_picker_bad_root_is_removed_before_due() {
        let result = validate(r#"{"modelPicker":{"options":null}}"#);
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        assert!(result
            .normalized
            .unwrap()
            .subprojection("/modelPicker")
            .is_err());
    }

    #[test]
    fn enabled_plugins_accepts_only_boolean_or_string_array_map_values() {
        for source in [
            r#"{"enabledPlugins":{"formatter@market":true}}"#,
            r#"{"enabledPlugins":{"formatter@market":["1.2.0","2.x"],"empty":[]}}"#,
        ] {
            assert_eq!(validate(source).status, NativeStaticSettingsStatus::Valid);
        }
        let invalid = validate(r#"{"enabledPlugins":{"formatter@market":{"enabled":true}}}"#);
        assert_eq!(invalid.status, NativeStaticSettingsStatus::Invalid);
        assert_eq!(
            invalid.due_issues[0].path.string_at(""),
            Some("enabledPlugins.formatter@market")
        );
    }

    #[test]
    fn model_pricing_at_drops_invalid_rows_and_due_strips_unknown_fields() {
        let result = validate(
            r#"{"modelPricing":{"multiplier":0,"overrides":{"good":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4,"extra":true},"bad":{"input":1,"output":2,"cacheRead":3,"cacheWrite":10001},"constructor":{"input":1,"output":2,"cacheRead":3,"cacheWrite":4}}}}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        let normalized = result.normalized.unwrap();
        assert!(normalized
            .subprojection("/modelPricing/multiplier")
            .is_err());
        assert!(normalized
            .subprojection("/modelPricing/overrides/bad")
            .is_err());
        assert!(normalized
            .subprojection("/modelPricing/overrides/constructor")
            .is_err());
        assert!(normalized
            .subprojection("/modelPricing/overrides/good/extra")
            .is_err());
        assert_eq!(
            normalized
                .at_pointer("/modelPricing/overrides/good/cacheWrite")
                .and_then(|value| value.as_number()),
            Some(4.0)
        );
    }

    #[test]
    fn mcp_and_marketplace_arrays_use_source_transforms_but_bad_roots_follow_due() {
        let result = validate(
            r#"{"allowedMcpServers":[{"serverName":"server_ok","extra":true},{"serverName":"bad/name"},{"serverName":"one","serverUrl":"https://example.test"}],"deniedMcpServers":[{"serverCommand":["mcp-server"]}],"strictKnownMarketplaces":[],"blockedMarketplaces":[],"extraKnownMarketplaces":{}}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        let normalized = result.normalized.unwrap();
        assert_eq!(normalized.array_len("/allowedMcpServers"), Some(1));
        assert_eq!(
            normalized.string_at("/allowedMcpServers/0/serverName"),
            Some("server_ok")
        );
        assert!(normalized
            .subprojection("/allowedMcpServers/0/extra")
            .is_err());
        assert_eq!(normalized.array_len("/deniedMcpServers"), Some(1));

        let bad_root = validate(r#"{"allowedMcpServers":"not-an-array"}"#);
        assert_eq!(bad_root.status, NativeStaticSettingsStatus::Invalid);
        assert!(bad_root.due_issues.iter().any(|issue| {
            issue.path.string_at("") == Some("allowedMcpServers")
                && issue.message.string_at("")
                    == Some("Invalid input: expected array, received string")
        }));
    }

    #[test]
    fn strict_plugin_surface_transform_and_empty_marketplace_maps_are_source_faithful() {
        let result = validate(
            r#"{"strictPluginOnlyCustomization":["skills","other",false,"mcp"],"managedMcpServers":null,"extraKnownMarketplaces":{},"strictKnownMarketplaces":[],"blockedMarketplaces":[]}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        let normalized = result.normalized.unwrap();
        assert_eq!(
            normalized.array_len("/strictPluginOnlyCustomization"),
            Some(2)
        );
        assert!(normalized.subprojection("/managedMcpServers").is_err());

        let rejected = validate(r#"{"strictPluginOnlyCustomization":{"bad":true}}"#);
        assert!(matches!(
            rejected.hard_rejection,
            Some(NativeStaticHardRejection::StrictPluginOnlyCustomization { .. })
        ));
    }

    #[test]
    fn ftr_hard_gate_order_is_strict_plugin_then_allowed_providers() {
        let result =
            validate(r#"{"strictPluginOnlyCustomization":"bad","allowedProviders":"bad"}"#);
        assert!(matches!(
            result.hard_rejection,
            Some(NativeStaticHardRejection::StrictPluginOnlyCustomization { .. })
        ));
        assert!(result.due_issues.is_empty());
    }

    #[test]
    fn allowed_providers_runs_due_catch_then_native_o_transform() {
        let custom = validate(r#"{"allowedProviders":["anthropicAcme"]}"#);
        assert_eq!(custom.status, NativeStaticSettingsStatus::Valid);
        assert_eq!(
            custom.normalized.unwrap().string_at("/allowedProviders/0"),
            Some("anthropicAcme")
        );

        let invalid = validate(r#"{"allowedProviders":["not-a-provider"]}"#);
        assert!(matches!(
            invalid.hard_rejection,
            Some(NativeStaticHardRejection::AllowedProvidersUnknownName { .. })
        ));
    }

    #[test]
    fn allowed_providers_o_uses_object_assign_insertion_order() {
        let source = parse(r#"{"allowedProviders":["anthropicAcme"]}"#);
        let replacement =
            Utf16JsonProjection::array(vec![Utf16JsonProjection::from("anthropicAcme")])
                .expect("provider list is an array");

        let existing = parse(r#"{"before":1,"allowedProviders":["old"],"after":3}"#);
        assert_eq!(
            assign_due_field_like_object_assign(existing, "allowedProviders", replacement.clone())
                .unwrap()
                .to_json_string()
                .unwrap(),
            r#"{"before":1,"allowedProviders":["anthropicAcme"],"after":3}"#
        );

        let missing = parse(r#"{"before":1,"after":3}"#);
        assert_eq!(
            apply_allowed_providers_o_transform(&source, missing)
                .to_json_string()
                .unwrap(),
            r#"{"before":1,"after":3,"allowedProviders":["anthropicAcme"]}"#
        );

        let due = parse(r#"{"before":1,"after":3}"#);
        for no_array_source in [
            parse(r#"{}"#),
            parse(r#"{"allowedProviders":null}"#),
            parse(r#"{"allowedProviders":"anthropicAcme"}"#),
        ] {
            assert_eq!(
                apply_allowed_providers_o_transform(&no_array_source, due.clone())
                    .to_json_string()
                    .unwrap(),
                r#"{"before":1,"after":3}"#
            );
        }
    }

    #[test]
    fn unknown_due_fields_pass_through_and_invalid_managed_mcp_rows_are_salvaged() {
        let pass = validate(r#"{"futureUnknown":{"keep":"source"}}"#);
        assert_eq!(pass.status, NativeStaticSettingsStatus::Valid);
        assert_eq!(
            pass.normalized.unwrap().string_at("/futureUnknown/keep"),
            Some("source")
        );

        let salvaged = validate(r#"{"managedMcpServers":{"server":{"type":"http"}}}"#);
        assert_eq!(salvaged.status, NativeStaticSettingsStatus::Valid);
        assert!(salvaged.unresolved_fields.is_empty());
        assert!(salvaged
            .normalized
            .as_ref()
            .and_then(|value| value.at_pointer("/managedMcpServers/server"))
            .is_none());
        assert_eq!(salvaged.managed_mcp_diagnostics.len(), 1);
        assert_eq!(salvaged.at_warnings.len(), 1);
        assert_eq!(
            salvaged.at_warnings[0].string_at("/path"),
            Some("managedMcpServers.server")
        );
        assert_eq!(
            salvaged.at_warnings[0].string_at("/message"),
            Some("Managed MCP server was ignored: Invalid input")
        );
        assert_eq!(
            salvaged.static_fallback_warnings[0]
                .at_pointer("/statusOnly")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
    }

    #[test]
    fn alias_warning_rows_match_ftr_and_static_tr_wrappers() {
        let result = validate(r#"{"allowedMarketplaces":[],"strictKnownMarketplaces":[]}"#);
        assert_eq!(result.at_warnings.len(), 1);
        assert_eq!(
            result.at_warnings[0].string_at("/message"),
            Some("\"allowedMarketplaces\" is an alias for \"strictKnownMarketplaces\" and this file sets both; the \"allowedMarketplaces\" value was ignored. Use only \"strictKnownMarketplaces\".")
        );
        assert_eq!(
            result.at_warnings[0]
                .at_pointer("/statusOnly")
                .and_then(JsonValueView::as_bool),
            None
        );
        assert_eq!(
            result.static_fallback_warnings[0]
                .at_pointer("/statusOnly")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
    }

    #[test]
    fn qn_log_messages_preserve_source_order_and_stripped_keys_precede_warnings() {
        let result = validate(
            r#"{"policyHelper":{},"allowedMarketplaces":[],"strictKnownMarketplaces":[]}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        let messages = native_qn_log_messages(&result, "policyHelpers.default")
            .expect("every At warning has a source message");
        let messages = messages
            .iter()
            .map(|message| message.string_at(""))
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            vec![
                Some("policyHelpers.default: stripped policyHelper from the settings payload (no recursion)"),
                Some("policyHelpers.default: \"allowedMarketplaces\" is an alias for \"strictKnownMarketplaces\" and this file sets both; the \"allowedMarketplaces\" value was ignored. Use only \"strictKnownMarketplaces\"."),
            ]
        );
        assert_eq!(result.static_fallback_warnings.len(), 1);
        assert_eq!(
            result.static_fallback_warnings[0]
                .at_pointer("/statusOnly")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
    }

    #[test]
    fn extra_marketplace_due_normalizes_go_rows_after_at_filters_name_mismatch() {
        let valid = validate(
            r#"{"extraKnownMarketplaces":{"acme":{"source":{"source":"settings","name":"acme","plugins":[],"unknownSource":true},"unknownRoot":true}}}"#,
        );
        assert_eq!(valid.status, NativeStaticSettingsStatus::Valid);
        assert_eq!(valid.unresolved_fields, Vec::<String>::new());
        assert_eq!(
            valid
                .normalized
                .as_ref()
                .and_then(|value| value.string_at("/extraKnownMarketplaces/acme/source/name")),
            Some("acme")
        );
        assert!(valid
            .normalized
            .as_ref()
            .and_then(|value| value.at_pointer("/extraKnownMarketplaces/acme/unknownRoot"))
            .is_none());
        assert!(valid
            .normalized
            .as_ref()
            .and_then(|value| value.at_pointer("/extraKnownMarketplaces/acme/source/unknownSource"))
            .is_none());

        let mismatch = validate(
            r#"{"extraKnownMarketplaces":{"key":{"source":{"source":"settings","name":"other","plugins":[]}}}}"#,
        );
        assert_eq!(mismatch.status, NativeStaticSettingsStatus::Valid);
        assert!(mismatch.due_issues.is_empty());
        assert_eq!(
            mismatch
                .normalized
                .as_ref()
                .and_then(|value| value.at_pointer("/extraKnownMarketplaces"))
                .and_then(JsonValueView::object_entries)
                .map(|entries| entries.len()),
            Some(0)
        );
        assert_eq!(
            mismatch.at_warnings[0].string_at("/path"),
            Some("extraKnownMarketplaces.key")
        );
        assert_eq!(
            mismatch.at_warnings[0].string_at("/message"),
            Some("Invalid marketplace entry was ignored: key \"key\" must match the settings source name \"other\"")
        );

        let invalid_row = validate(
            r#"{"extraKnownMarketplaces":{"key":{"source":{"source":"github","repo":7}}}}"#,
        );
        assert_eq!(invalid_row.status, NativeStaticSettingsStatus::Valid);
        assert!(invalid_row.due_issues.is_empty());
        assert_eq!(
            invalid_row
                .normalized
                .as_ref()
                .and_then(|value| value.at_pointer("/extraKnownMarketplaces"))
                .and_then(JsonValueView::object_entries)
                .map(|entries| entries.len()),
            Some(0)
        );
        assert_eq!(invalid_row.at_warnings.len(), 1);
    }

    #[test]
    fn extra_marketplace_at_warning_uses_native_safe_key_for_lone_surrogate() {
        let result = validate(
            r#"{"extraKnownMarketplaces":{"\ud800":{"source":{"source":"settings","name":"different","plugins":[]}}}}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Valid);
        assert!(result.due_issues.is_empty());
        assert_eq!(
            result.at_warnings[0].string_at("/path"),
            Some("extraKnownMarketplaces.<key>")
        );
        assert_eq!(
            result.at_warnings[0].string_at("/message"),
            Some("Invalid marketplace entry was ignored: key \"<key>\" must match the settings source name \"different\"")
        );
        assert_eq!(
            result
                .normalized
                .as_ref()
                .and_then(|value| value.at_pointer("/extraKnownMarketplaces"))
                .and_then(JsonValueView::object_entries)
                .map(|entries| entries.len()),
            Some(0)
        );
    }

    #[test]
    fn due_issues_follow_registry_order_not_input_order() {
        let result = validate(r#"{"defaultShell":"invalid","workflowSizeGuideline":"invalid"}"#);
        assert_eq!(result.status, NativeStaticSettingsStatus::Invalid);
        assert_eq!(
            result.due_issues[0].path.string_at(""),
            Some("workflowSizeGuideline")
        );
        assert_eq!(
            result.due_issues[1].path.string_at(""),
            Some("defaultShell")
        );
        assert_eq!(result.raw.string_at("/defaultShell"), Some("invalid"));
    }

    #[test]
    fn source_diagnostic_formats_i5e_hard_and_due_failures() {
        let i5e = validate(r#"{"path":"/tmp/ignored"}"#);
        let diagnostic = native_static_settings_diagnostic(&i5e, "policyHelpers.default")
            .expect("I5e failure has a source-bound fatal message");
        assert_eq!(
            diagnostic.kind,
            NativeStaticSettingsDiagnosticKind::I5eStartupFatal
        );
        assert_eq!(
            diagnostic.message.string_at(""),
            Some("policyHelpers.default is not a valid static settings payload (must not contain \"path\" — a static payload is a managed-settings object, not a policyHelpers entry; entry fields (path/script/interpreter/outputBehavior/onFailure/retries/timeoutMs/refreshIntervalMs/defaultSettings) belong on the per-OS entries (policyHelpers.macos/linux/windows/wsl)); Claude Code will not start until it is fixed")
        );

        let hard = validate(r#"{"allowedProviders":["anthropic","not-a-provider"]}"#);
        let diagnostic = native_static_settings_diagnostic(&hard, "policyHelpers.default")
            .expect("hTr/yTr failure has a source-bound fTr message");
        assert_eq!(
            diagnostic.kind,
            NativeStaticSettingsDiagnosticKind::AtHardRejection
        );
        assert_eq!(
            diagnostic.message.string_at(""),
            Some("policyHelpers.default is not a valid static settings payload (payload rejected: allowedProviders: unknown provider name \"not-a-provider\"; known names: anthropic, customEndpoint, bedrock, vertex, foundry, anthropicAws, mantle, gateway); Claude Code will not start until it is fixed")
        );

        let due = validate(r#"{"permissions":{"defaultMode":null}}"#);
        let diagnostic = native_static_settings_diagnostic(&due, "policyHelpers.default")
            .expect("Due failure has a source-bound fTr message");
        assert_eq!(
            diagnostic.kind,
            NativeStaticSettingsDiagnosticKind::DueRejection
        );
        assert_eq!(
            diagnostic.message.string_at(""),
            Some("policyHelpers.default is not a valid static settings payload (payload rejected: permissions.defaultMode: Invalid option: expected one of \"acceptEdits\"|\"auto\"|\"bypassPermissions\"|\"default\"|\"dontAsk\"|\"plan\"); Claude Code will not start until it is fixed")
        );
    }

    #[test]
    fn due_diagnostic_keeps_lone_surrogate_dynamic_record_keys() {
        let source = r#"{"pluginConfigs":{"\ud800":{"options":{"x":{}}}}}"#;
        let raw = Utf16JsonProjection::parse_utf16_json(&source.encode_utf16().collect::<Vec<_>>())
            .expect("escaped lone surrogate is valid source JSON");
        let result = validate_native_static_default_settings(&raw, "policyHelpers.default", &[]);
        assert_eq!(result.status, NativeStaticSettingsStatus::Invalid);
        assert!(result.unresolved_fields.is_empty());
        let issue = result
            .due_issues
            .first()
            .expect("invalid plugin config is a Due issue");
        let path_units = issue
            .path
            .at_pointer("")
            .and_then(crate::types::utf16_json::JsonValueView::code_units)
            .expect("path is a source string");
        assert!(path_units.contains(&0xd800));
        assert_eq!(
            path_units,
            [
                "pluginConfigs.".encode_utf16().collect::<Vec<_>>(),
                vec![0xd800],
                ".options.x".encode_utf16().collect::<Vec<_>>(),
            ]
            .concat()
        );
        let diagnostic = native_static_settings_diagnostic(&result, "policyHelpers.default")
            .expect("first-three formatter retains the source path units");
        let message_units = diagnostic
            .message
            .at_pointer("")
            .and_then(crate::types::utf16_json::JsonValueView::code_units)
            .expect("diagnostic is a source string");
        assert!(message_units.contains(&0xd800));
    }

    #[test]
    fn due_error_formatter_refuses_to_claim_first_three_without_regex_worker_rows() {
        let result = validate(
            r#"{"sandbox":{"credentials":{"files":[{"path":"/tmp/key","mode":"mask","extract":"secret=(.*)"}]}},"defaultShell":false,"workflowSizeGuideline":"bad"}"#,
        );
        assert_eq!(result.status, NativeStaticSettingsStatus::Invalid);
        assert!(result
            .unresolved_fields
            .iter()
            .any(|field| field == "sandbox.credentials.files.0.extract/js-regexp-result-missing"));
        assert!(native_static_settings_diagnostic(&result, "policyHelpers.default").is_none());
    }

    #[test]
    fn hooks_run_through_at_and_nested_due_before_normalized_candidate_is_admitted() {
        let valid = validate(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo ok"}]}]}}"#,
        );
        assert_eq!(valid.status, NativeStaticSettingsStatus::Valid);
        assert_eq!(
            valid
                .normalized
                .as_ref()
                .and_then(|projection| projection.at_pointer("/hooks/PreToolUse/0/hooks/0/command"))
                .and_then(JsonValueView::as_str),
            Some("echo ok")
        );

        let invalid = validate(
            r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command"}]}]}}"#,
        );
        assert_eq!(invalid.status, NativeStaticSettingsStatus::Invalid);
        assert!(invalid.unresolved_fields.is_empty());
        assert!(invalid.due_issues.iter().any(|issue| issue
            .path
            .string_at("")
            .is_some_and(|path| path.starts_with("hooks."))));
        assert!(native_static_settings_diagnostic(&invalid, "policyHelpers.default").is_some());
    }
}
