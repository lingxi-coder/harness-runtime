//! Source-aware normalization for the supported subset of Native managed policy.
//!
//! This is deliberately separate from [`super::schema::SettingsJson`]. Native's
//! managed-policy parser catches and repairs several fields independently;
//! ordinary settings-file admission remains unchanged. The result therefore
//! preserves the input and exposes exactly which fields this module normalized
//! and which Native policy parsers still need implementation.
//!
//! Source binding: `native-nl-source/src_187673145.js` (chunk SHA-256
//! `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`),
//! `Qo` at local byte 277602, `Bo` at 286081, `os` at 287594, and `pn` at
//! 280595. The tests below are fixture oracles for those source branches; they
//! do not execute Native.

use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Native managed-policy paths normalized by this module.
///
/// All other fields remain in the passthrough projection. A field can still be
/// listed in [`UNRESOLVED_NATIVE_POLICY_FIELDS`] when its Native-specific
/// validator or fallback is not implemented here.
pub const COVERED_NATIVE_POLICY_FIELDS: &[&str] = &[
    "availableModels",
    "enforceAvailableModels",
    "allowedHttpHookUrls",
    "httpHookAllowedEnvVars",
    "managedSourcesBehavior",
    "parentSettingsBehavior",
    "wslInheritsWindowsSettings",
    "sandbox",
    "sandbox.enabled",
    "sandbox.failIfUnavailable",
    "sandbox.autoAllowBashIfSandboxed",
    "sandbox.allowUnsandboxedCommands",
    "sandbox.enableWeakerNestedSandbox",
    "sandbox.enableWeakerNetworkIsolation",
    "sandbox.allowAppleEvents",
    "sandbox.network.allowManagedDomainsOnly",
    "sandbox.network.strictAllowlist",
    "sandbox.network.allowAllUnixSockets",
    "sandbox.network.allowLocalBinding",
    "sandbox.filesystem.allowManagedReadPathsOnly",
    "sandbox.filesystem.disabled",
    "allowManagedPermissionRulesOnly",
    "allowManagedHooksOnly",
    "allowManagedMcpServersOnly",
    "disableClaudeAiConnectors",
    "disableCommandPluginSources",
    "disableSideloadFlags",
    "disableSkillShellExecution",
    "disableRemoteControl",
    "disableAgentView",
    "disableWorkflows",
    "disableArtifact",
    "enableArtifact",
    "enableWorkflows",
    "disableBundledSkills",
    "fastModePerSessionOptIn",
    "isolatePeerMachines",
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
    "disableAutoMode",
    "disableDeepLinkRegistration",
];

/// Native managed-policy parsers with source-specific normalization not yet
/// represented by this module. Present fields are preserved, not claimed as
/// validated or normalized.
pub const UNRESOLVED_NATIVE_POLICY_FIELDS: &[&str] = &[
    "prependPlugins",
    "appendPlugins",
    "strictKnownMarketplaces",
    "blockedMarketplaces",
    "allowedMcpServers",
    "deniedMcpServers",
    "managedMcpServers",
    "disableAllHooks",
    "strictPluginOnlyCustomization",
    "enabledPlugins",
    "deniedModels",
    "availableModelsMatch",
    "modelProposedGoals",
    "feedbackDrafts",
    "askUserQuestionTimeout",
    "dialogExpiry",
    "allowedChannelPlugins",
    "gatewayInternalNetworks",
    "forceLoginOrgUUID",
    "allowedProviders",
    "policyHelper",
    "policyHelpers",
    "permissions.disableBypassPermissionsMode",
    "permissions.disableAutoMode",
    "permissions.blockReadsOutsideWorkingDirectories",
    "autoMode.classifyAllShell",
    "worktree.bgIsolation",
    "remoteTools.allowUnattendedServing",
    "attribution.sessionUrl",
    "crossSessionInbound",
    "remoteControl.shareHostProfile",
    "isolation.required",
    "isolation.persistHome",
];

/// Source facts that affect Native policy parsing before field normalization.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativePolicyParseOptions {
    /// Whether the plist/registry candidate is user-writable. Native removes
    /// admin-only fields before parsing such candidates.
    pub user_writable: bool,
}

/// Structural status for one policy document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyParseStatus {
    /// The input was not valid JSON. The raw text is still retained.
    RejectedInvalidJson(String),
    /// JSON parsed, but the root was not an object.
    RejectedNonObjectRoot,
    /// JSON and object-root admission succeeded. The unresolved-field list
    /// says whether the whole Native field schema is represented here.
    ParsedObject,
}

/// Classification of one Native-shaped diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativePolicyDiagnosticKind {
    /// A value was coerced without changing whether the policy applies.
    Coerced,
    /// One invalid array member was ignored while valid siblings remained.
    InvalidEntryIgnored,
    /// An invalid array field was replaced with an empty fail-closed list.
    FallbackEmpty,
    /// An invalid field was ignored or removed.
    Ignored,
    /// Native substituted a restrictive value for an invalid field.
    Substituted,
    /// Native's final transform found that every effective policy value was a
    /// substitute or synthesized skeleton.
    OnlySubstitutes,
    /// Native synthesized a fail-closed object skeleton.
    Synthesized,
    /// An explicit null was interpreted as key removal.
    Removed,
    /// A Native-specific parser for this field is not implemented here.
    Unresolved,
}

/// One diagnostic emitted while normalizing a managed-policy projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyParseDiagnostic {
    /// JSON Pointer of the affected field or array member.
    pub path: String,
    /// Native-shaped outcome category.
    pub kind: NativePolicyDiagnosticKind,
    /// Native marks this diagnostic as informational/status-only.
    pub status_only: bool,
    /// Whether Native marks this diagnostic as a restrictive substitution.
    pub substituted: bool,
    /// Whether the affected value was synthesized as a fallback skeleton.
    pub synthesized: bool,
    /// Whether the final document contains only substitute/synthesized policy
    /// content and Native marks this diagnostic `onlySubstitutes`.
    pub only_substitutes: bool,
}

/// Raw and normalized views of one managed-policy document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyParseResult {
    /// Exact raw UTF-8 document text, including text that fails JSON parsing.
    pub raw_document: String,
    /// Raw parsed JSON projection, when the input is valid JSON.
    pub raw_projection: Option<Utf16JsonProjection>,
    /// Projection after the Native user-writable source prefilter.
    pub candidate_projection: Option<Utf16JsonProjection>,
    /// Candidate with supported Native field rules applied. Unresolved fields
    /// remain untouched in this projection and are listed separately.
    pub normalized_projection: Option<Utf16JsonProjection>,
    /// Parse status for the complete document.
    pub status: NativePolicyParseStatus,
    /// Whether JSON and object-root admission succeeded. This is structural
    /// admission only; it does not claim every Native field validator is
    /// implemented. Consult [`Self::unresolved_fields`] before treating the
    /// projection as a complete Native schema result.
    pub document_admitted: bool,
    /// Native `documentHasPolicyContent` when known from normalized fields or
    /// passthrough fields; `None` means an unresolved field prevents a claim.
    pub document_has_policy_content: Option<bool>,
    /// Native `onlySubstitutes` result for the normalized fields represented
    /// here.
    pub only_substitutes: bool,
    /// Field-level source diagnostics in traversal order.
    pub diagnostics: Vec<NativePolicyParseDiagnostic>,
    /// Paths for which Native emitted a restrictive substitute.
    pub substituted_paths: Vec<String>,
    /// Paths for which Native synthesized a fail-closed skeleton.
    pub synthesized_paths: Vec<String>,
    /// Present Native-special fields whose field-level parser is not yet
    /// represented by this module.
    pub unresolved_fields: Vec<String>,
}

/// Parse one managed-policy JSON document and normalize the covered Native
/// field rules.
///
/// `document_admitted` refers only to valid JSON with an object root. This
/// function intentionally does not use the ordinary `SettingsJson` parser as a
/// whole-document gate: Native repairs selected fields independently, and the
/// unported Native field validators are reported in `unresolved_fields`.
#[must_use]
pub fn parse_policy_document(
    raw: &str,
    options: NativePolicyParseOptions,
) -> NativePolicyParseResult {
    let raw_projection = match Utf16JsonProjection::parse(raw) {
        Ok(projection) => projection,
        Err(error) => {
            return NativePolicyParseResult {
                raw_document: raw.to_owned(),
                raw_projection: None,
                candidate_projection: None,
                normalized_projection: None,
                status: NativePolicyParseStatus::RejectedInvalidJson(error.to_string()),
                document_admitted: false,
                document_has_policy_content: Some(false),
                only_substitutes: false,
                diagnostics: Vec::new(),
                substituted_paths: Vec::new(),
                synthesized_paths: Vec::new(),
                unresolved_fields: Vec::new(),
            };
        }
    };

    if raw_projection.object_entries().is_none() {
        return NativePolicyParseResult {
            raw_document: raw.to_owned(),
            raw_projection: Some(raw_projection),
            candidate_projection: None,
            normalized_projection: None,
            status: NativePolicyParseStatus::RejectedNonObjectRoot,
            document_admitted: false,
            document_has_policy_content: Some(false),
            only_substitutes: false,
            diagnostics: Vec::new(),
            substituted_paths: Vec::new(),
            synthesized_paths: Vec::new(),
            unresolved_fields: Vec::new(),
        };
    }

    let mut candidate_projection = raw_projection.clone();
    if options.user_writable {
        filter_user_writable_candidate(&mut candidate_projection);
    }

    let mut normalized_projection = candidate_projection.clone();
    let mut diagnostics = Vec::new();
    let mut substituted_paths = Vec::new();
    let mut synthesized_paths = Vec::new();

    normalize_available_models(&mut normalized_projection, &mut diagnostics);
    normalize_string_allowlist(
        &mut normalized_projection,
        "allowedHttpHookUrls",
        &mut diagnostics,
    );
    normalize_string_allowlist(
        &mut normalized_projection,
        "httpHookAllowedEnvVars",
        &mut diagnostics,
    );
    normalize_behavior_control(
        &mut normalized_projection,
        "managedSourcesBehavior",
        &mut diagnostics,
    );
    normalize_behavior_control(
        &mut normalized_projection,
        "parentSettingsBehavior",
        &mut diagnostics,
    );
    normalize_wsl_control(&mut normalized_projection, &mut diagnostics);
    normalize_root_restrictions(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
    );
    normalize_sandbox(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
        &mut synthesized_paths,
    );

    let unresolved_fields = collect_unresolved_fields(&candidate_projection);
    for field in &unresolved_fields {
        diagnostics.push(NativePolicyParseDiagnostic {
            path: format!("/{field}"),
            kind: NativePolicyDiagnosticKind::Unresolved,
            status_only: false,
            substituted: false,
            synthesized: false,
            only_substitutes: false,
        });
    }

    let only_substitutes = only_substitutes(
        &normalized_projection,
        &substituted_paths,
        &synthesized_paths,
    );
    if only_substitutes {
        if let Some(entries) = normalized_projection.object_entries() {
            for entry in entries {
                let root = entry.display_key;
                let path = format!("/{root}");
                if is_control_key(entry.code_units) || container_only(entry.value) {
                    continue;
                }
                if substituted_paths.iter().any(|value| value == &path)
                    || synthesized_paths.iter().any(|value| value == &path)
                    || is_substitute_tree(
                        entry.value,
                        &path,
                        &substituted_paths,
                        &synthesized_paths,
                    )
                {
                    diagnostics.push(diagnostic(
                        path,
                        NativePolicyDiagnosticKind::OnlySubstitutes,
                        false,
                        false,
                        true,
                    ));
                }
            }
        }
    }

    let document_has_policy_content =
        policy_content_status(&normalized_projection, &unresolved_fields);

    NativePolicyParseResult {
        raw_document: raw.to_owned(),
        raw_projection: Some(raw_projection),
        candidate_projection: Some(candidate_projection),
        normalized_projection: Some(normalized_projection),
        status: NativePolicyParseStatus::ParsedObject,
        document_admitted: true,
        document_has_policy_content,
        only_substitutes,
        diagnostics,
        substituted_paths,
        synthesized_paths,
        unresolved_fields,
    }
}

const ROOT_RESTRICTIVE_BOOLEANS: &[(&str, bool)] = &[
    ("allowManagedPermissionRulesOnly", true),
    ("allowManagedHooksOnly", true),
    ("allowManagedMcpServersOnly", true),
    ("enforceAvailableModels", true),
    ("disableClaudeAiConnectors", true),
    ("disableCommandPluginSources", true),
    ("disableSideloadFlags", true),
    ("disableSkillShellExecution", true),
    ("disableRemoteControl", true),
    ("disableAgentView", true),
    ("disableWorkflows", true),
    ("disableArtifact", true),
    ("disableBundledSkills", true),
    ("fastModePerSessionOptIn", true),
    ("isolatePeerMachines", true),
    ("enableArtifact", false),
    ("enableWorkflows", false),
    ("syncClaudeAiSkills", false),
    ("syncClaudeAiPlugins", false),
    ("useAutoModeDuringPlan", false),
    ("skipDangerousModePermissionPrompt", false),
    ("skipAutoPermissionPrompt", false),
    ("enableAllProjectMcpServers", false),
    ("channelsEnabled", false),
    ("skipWebFetchPreflight", false),
    ("skipWorkflowUsageWarning", false),
    ("autoUploadSessions", false),
    ("remoteControlAtStartup", false),
    ("autoContinueAtUsageLimit", false),
    ("disablePluginDirectory", true),
];

fn normalize_root_restrictions(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
) {
    for (field, restrictive) in ROOT_RESTRICTIVE_BOOLEANS {
        normalize_bool_field(
            projection,
            &[*field],
            Some(*restrictive),
            diagnostics,
            substituted_paths,
        );
    }
    for field in ["disableAutoMode", "disableDeepLinkRegistration"] {
        normalize_disable_only_field(projection, field, diagnostics, substituted_paths);
    }
}

fn normalize_available_models(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    normalize_string_array(projection, "availableModels", false, diagnostics);
}

fn normalize_string_allowlist(
    projection: &mut Utf16JsonProjection,
    field: &str,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    normalize_string_array(projection, field, true, diagnostics);
}

fn normalize_string_array(
    projection: &mut Utf16JsonProjection,
    field: &str,
    report_all_invalid: bool,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    let Some(value) = projection.subprojection(&format!("/{field}")).ok() else {
        return;
    };
    let Some(items) = value.at_pointer("").and_then(JsonValueView::array_len) else {
        let _ = projection.set_field(field, empty_array());
        diagnostics.push(diagnostic(
            format!("/{field}"),
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
        return;
    };

    let mut retained = Vec::with_capacity(items);
    for index in 0..items {
        let pointer = format!("/{index}");
        let Some(item) = value.subprojection(&pointer).ok() else {
            continue;
        };
        if item
            .at_pointer("")
            .and_then(JsonValueView::as_str)
            .is_some()
        {
            retained.push(item);
        } else {
            diagnostics.push(diagnostic(
                format!("/{field}/{index}"),
                NativePolicyDiagnosticKind::InvalidEntryIgnored,
                false,
                false,
                false,
            ));
        }
    }

    if report_all_invalid && items > 0 && retained.is_empty() {
        diagnostics.push(diagnostic(
            format!("/{field}"),
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
    }
    let Ok(array) = Utf16JsonProjection::array(retained) else {
        return;
    };
    let _ = projection.set_field(field, array);
}

fn normalize_behavior_control(
    projection: &mut Utf16JsonProjection,
    field: &str,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    let pointer = format!("/{field}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let valid = value
        .at_pointer("")
        .and_then(JsonValueView::as_str)
        .is_some_and(|value| matches!(value, "first-wins" | "merge"));
    if valid {
        return;
    }
    let _ = projection.remove_field(field);
    diagnostics.push(diagnostic(
        pointer,
        NativePolicyDiagnosticKind::Ignored,
        false,
        false,
        false,
    ));
}

fn normalize_wsl_control(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    const FIELD: &str = "wslInheritsWindowsSettings";
    let pointer = format!("/{FIELD}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.is_null() {
        let _ = projection.remove_field(FIELD);
        return;
    }
    if root.as_bool().is_some() {
        return;
    }
    if let Some(boolean) = root.as_str().and_then(|value| match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }) {
        let _ = projection.set_field(FIELD, bool_projection(boolean));
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Coerced,
            false,
            false,
            false,
        ));
        return;
    }
    let _ = projection.remove_field(FIELD);
    diagnostics.push(diagnostic(
        pointer,
        NativePolicyDiagnosticKind::Ignored,
        false,
        false,
        false,
    ));
}

fn normalize_bool_field(
    projection: &mut Utf16JsonProjection,
    path: &[&str],
    invalid_fallback: Option<bool>,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
) {
    let pointer = json_pointer(path);
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.is_null() {
        remove_path(projection, path);
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Removed,
            false,
            false,
            false,
        ));
        return;
    }
    if root.as_bool().is_some() {
        return;
    }
    if let Some(boolean) = root.as_str().and_then(|value| match value {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }) {
        set_path(projection, path, bool_projection(boolean));
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Coerced,
            false,
            false,
            false,
        ));
        return;
    }

    if let Some(fallback) = invalid_fallback {
        set_path(projection, path, bool_projection(fallback));
        substituted_paths.push(pointer.clone());
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Substituted,
            true,
            false,
            false,
        ));
    } else {
        remove_path(projection, path);
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Ignored,
            false,
            false,
            false,
        ));
    }
}

fn normalize_disable_only_field(
    projection: &mut Utf16JsonProjection,
    field: &str,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
) {
    let pointer = format!("/{field}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.is_null() || root.as_bool() == Some(false) || root.as_str() == Some("false") {
        let _ = projection.remove_field(field);
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Removed,
            false,
            false,
            false,
        ));
        return;
    }
    if root.as_str() == Some("disable") {
        return;
    }
    let _ = projection.set_field(field, string_projection("disable"));
    substituted_paths.push(pointer.clone());
    diagnostics.push(diagnostic(
        pointer,
        NativePolicyDiagnosticKind::Substituted,
        true,
        false,
        false,
    ));
}

fn normalize_sandbox(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
    synthesized_paths: &mut Vec<String>,
) {
    let Some(sandbox) = projection.subprojection("/sandbox").ok() else {
        return;
    };
    let Some(root) = sandbox.at_pointer("") else {
        return;
    };
    if root.is_null() {
        let _ = projection.remove_field("sandbox");
        diagnostics.push(diagnostic(
            "/sandbox".into(),
            NativePolicyDiagnosticKind::Removed,
            false,
            false,
            false,
        ));
        return;
    }
    if root.object_entries().is_none() {
        let skeleton = sandbox_skeleton();
        let _ = projection.set_field("sandbox", skeleton);
        substituted_paths.push("/sandbox".into());
        synthesized_paths.push("/sandbox".into());
        diagnostics.push(diagnostic(
            "/sandbox".into(),
            NativePolicyDiagnosticKind::Synthesized,
            true,
            true,
            false,
        ));
        return;
    }

    for (field, fallback) in [
        ("enabled", Some(true)),
        ("failIfUnavailable", None),
        ("autoAllowBashIfSandboxed", Some(false)),
        ("allowUnsandboxedCommands", Some(false)),
        ("enableWeakerNestedSandbox", Some(false)),
        ("enableWeakerNetworkIsolation", Some(false)),
        ("allowAppleEvents", Some(false)),
    ] {
        normalize_bool_field(
            projection,
            &["sandbox", field],
            fallback,
            diagnostics,
            substituted_paths,
        );
    }

    normalize_restricted_sandbox_object(
        projection,
        "network",
        &[
            "strictAllowlist",
            "allowManagedDomainsOnly",
            "allowAllUnixSockets",
            "allowLocalBinding",
        ],
        &[true, true, false, false],
        diagnostics,
        substituted_paths,
        synthesized_paths,
    );
    normalize_restricted_sandbox_object(
        projection,
        "filesystem",
        &["allowManagedReadPathsOnly", "disabled"],
        &[true, false],
        diagnostics,
        substituted_paths,
        synthesized_paths,
    );
    if let Some(sandbox) = projection.subprojection("/sandbox").ok() {
        if sandbox
            .at_pointer("")
            .and_then(JsonValueView::object_entries)
            .is_some_and(|entries| entries.is_empty())
        {
            let _ = projection.remove_field("sandbox");
        }
    }
}

fn normalize_restricted_sandbox_object(
    projection: &mut Utf16JsonProjection,
    object: &str,
    fields: &[&str],
    fallbacks: &[bool],
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
    synthesized_paths: &mut Vec<String>,
) {
    let path = ["sandbox", object];
    let pointer = json_pointer(&path);
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.is_null() {
        remove_path(projection, &path);
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Removed,
            false,
            false,
            false,
        ));
        return;
    }
    if root.object_entries().is_none() {
        let skeleton = bool_skeleton(fields, fallbacks);
        set_path(projection, &path, skeleton);
        substituted_paths.push(pointer.clone());
        synthesized_paths.push(pointer.clone());
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Synthesized,
            true,
            true,
            false,
        ));
        return;
    }
    for (field, fallback) in fields.iter().zip(fallbacks) {
        normalize_bool_field(
            projection,
            &["sandbox", object, field],
            Some(*fallback),
            diagnostics,
            substituted_paths,
        );
    }
}

fn filter_user_writable_candidate(projection: &mut Utf16JsonProjection) {
    for field in [
        "managedMcpServers",
        "isolation",
        "prependPlugins",
        "appendPlugins",
    ] {
        let _ = projection.remove_field(field);
    }
    let Some(enabled_plugins) = projection.subprojection("/enabledPlugins").ok() else {
        return;
    };
    let Some(entries) = enabled_plugins
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
    else {
        return;
    };
    let retained = entries
        .into_iter()
        .filter(|entry| {
            !entry
                .code_units
                .ends_with(&"@builtin".encode_utf16().collect::<Vec<_>>())
                && entry.value.as_bool() == Some(false)
        })
        .filter_map(|entry| {
            enabled_plugins
                .subprojection_property_units(entry.code_units)
                .ok()
                .map(|value| (entry.code_units.to_vec(), value))
        })
        .collect::<Vec<_>>();
    if retained.is_empty() {
        let _ = projection.remove_field("enabledPlugins");
    } else if let Ok(filtered) = Utf16JsonProjection::object_units(retained) {
        let _ = projection.set_field("enabledPlugins", filtered);
    }
}

fn collect_unresolved_fields(projection: &Utf16JsonProjection) -> Vec<String> {
    let mut unresolved = Vec::new();
    for field in UNRESOLVED_NATIVE_POLICY_FIELDS {
        if COVERED_NATIVE_POLICY_FIELDS.contains(field) {
            continue;
        }
        let pointer = format!("/{}", field.replace('.', "/"));
        if projection.subprojection(&pointer).is_ok() {
            unresolved.push((*field).to_owned());
        }
    }
    if let Some(sandbox) = projection.subprojection("/sandbox").ok() {
        for field in [
            "excludedCommands",
            "ripgrep",
            "ignoreViolations",
            "bwrapPath",
            "socatPath",
            "credentials",
            "enabledPlatforms",
        ] {
            if sandbox.contains_property(field) {
                unresolved.push(format!("sandbox.{field}"));
            }
        }
        for (object, fields) in [
            (
                "network",
                &[
                    "allowedDomains",
                    "deniedDomains",
                    "allowUnixSockets",
                    "allowMachLookup",
                    "httpProxyPort",
                    "socksProxyPort",
                    "tlsTerminate",
                ][..],
            ),
            (
                "filesystem",
                &["allowRead", "denyRead", "allowWrite", "denyWrite"][..],
            ),
        ] {
            if let Some(child) = sandbox.subprojection(&format!("/{object}")).ok() {
                for field in fields {
                    if child.contains_property(field) {
                        unresolved.push(format!("sandbox.{object}.{field}"));
                    }
                }
            }
        }
    }
    unresolved.sort();
    unresolved.dedup();
    unresolved
}

fn only_substitutes(
    projection: &Utf16JsonProjection,
    substituted_paths: &[String],
    synthesized_paths: &[String],
) -> bool {
    let Some(entries) = projection.object_entries() else {
        return false;
    };
    let meaningful = entries
        .into_iter()
        .filter(|entry| !is_control_key(entry.code_units))
        .filter(|entry| !container_only(entry.value));
    let mut found = false;
    for entry in meaningful {
        found = true;
        let top_path = format!("/{}", entry.display_key);
        if !substituted_paths.iter().any(|path| path == &top_path)
            && !synthesized_paths.iter().any(|path| path == &top_path)
            && !is_substitute_tree(entry.value, &top_path, substituted_paths, synthesized_paths)
        {
            return false;
        }
    }
    found
}

fn is_substitute_tree(
    value: JsonValueView<'_>,
    path: &str,
    substituted_paths: &[String],
    synthesized_paths: &[String],
) -> bool {
    if substituted_paths.iter().any(|candidate| candidate == path)
        || synthesized_paths.iter().any(|candidate| candidate == path)
    {
        return true;
    }
    let Some(entries) = value.object_entries() else {
        return false;
    };
    entries.iter().all(|entry| {
        let child_path = format!("{path}/{}", escape_pointer_token(entry.display_key));
        container_only(entry.value)
            || is_substitute_tree(
                entry.value,
                &child_path,
                substituted_paths,
                synthesized_paths,
            )
    })
}

fn container_only(value: JsonValueView<'_>) -> bool {
    if let Some(entries) = value.object_entries() {
        return entries.iter().all(|entry| container_only(entry.value));
    }
    if let Some(length) = value.array_len() {
        return (0..length)
            .filter_map(|index| value.get_index(index))
            .all(container_only);
    }
    false
}

fn policy_content_status(
    normalized: &Utf16JsonProjection,
    unresolved_fields: &[String],
) -> Option<bool> {
    let entries = normalized.object_entries()?;
    let mut unresolved_content = false;
    for entry in entries {
        if is_control_key(entry.code_units) || entry.value.is_null() {
            continue;
        }
        if unresolved_fields.iter().any(|field| {
            field == entry.display_key || field.starts_with(&format!("{}.", entry.display_key))
        }) {
            unresolved_content = true;
        } else {
            // Native documentHasPolicyContent is a key-presence floor check,
            // not the non-empty-content test used by onlySubstitutes. For
            // example, availableModels: [] still means "only the default
            // model" and must keep the policy document in the chain.
            return Some(true);
        }
    }
    if unresolved_content {
        None
    } else {
        Some(false)
    }
}

fn is_control_key(units: &[u16]) -> bool {
    units == "managedSourcesBehavior".encode_utf16().collect::<Vec<_>>()
        || units
            == "wslInheritsWindowsSettings"
                .encode_utf16()
                .collect::<Vec<_>>()
}

fn diagnostic(
    path: String,
    kind: NativePolicyDiagnosticKind,
    substituted: bool,
    synthesized: bool,
    only_substitutes: bool,
) -> NativePolicyParseDiagnostic {
    NativePolicyParseDiagnostic {
        path,
        kind,
        status_only: matches!(
            kind,
            NativePolicyDiagnosticKind::Coerced
                | NativePolicyDiagnosticKind::Removed
                | NativePolicyDiagnosticKind::OnlySubstitutes
        ),
        substituted,
        synthesized,
        only_substitutes,
    }
}

fn bool_skeleton(fields: &[&str], values: &[bool]) -> Utf16JsonProjection {
    Utf16JsonProjection::object(
        fields
            .iter()
            .zip(values)
            .map(|(field, value)| ((*field).to_owned(), bool_projection(*value)))
            .collect(),
    )
    .expect("restricted object skeleton is valid JSON")
}

fn sandbox_skeleton() -> Utf16JsonProjection {
    Utf16JsonProjection::parse(
        r#"{"autoAllowBashIfSandboxed":false,"allowUnsandboxedCommands":false,"network":{"strictAllowlist":true,"allowManagedDomainsOnly":true,"allowAllUnixSockets":false,"allowLocalBinding":false},"filesystem":{"allowManagedReadPathsOnly":true,"disabled":false},"credentials":{"allowPlaintextInject":false,"sigv4":{"streaming":"deny","presigned":"deny","sigv4a":"deny"}},"enableWeakerNestedSandbox":false,"enableWeakerNetworkIsolation":false,"allowAppleEvents":false}"#,
    )
    .expect("Native restrictive sandbox skeleton is valid JSON")
}

fn set_path(projection: &mut Utf16JsonProjection, path: &[&str], replacement: Utf16JsonProjection) {
    if path.len() == 1 {
        let _ = projection.set_field(path[0], replacement);
        return;
    }
    let parent_path = json_pointer(&path[..path.len() - 1]);
    let Ok(mut parent) = projection.subprojection(&parent_path) else {
        return;
    };
    if parent.set_field(path[path.len() - 1], replacement).is_ok() {
        let _ = projection.replace_pointer_projection(&parent_path, parent);
    }
}

fn remove_path(projection: &mut Utf16JsonProjection, path: &[&str]) {
    if path.len() == 1 {
        let _ = projection.remove_field(path[0]);
        return;
    }
    let parent_path = json_pointer(&path[..path.len() - 1]);
    let Ok(mut parent) = projection.subprojection(&parent_path) else {
        return;
    };
    let _ = parent.remove_field(path[path.len() - 1]);
    if parent
        .at_pointer("")
        .and_then(JsonValueView::object_entries)
        .is_some_and(|entries| entries.is_empty())
    {
        remove_path(projection, &path[..path.len() - 1]);
    } else {
        let _ = projection.replace_pointer_projection(&parent_path, parent);
    }
}

fn json_pointer(path: &[&str]) -> String {
    path.iter()
        .map(|part| format!("/{}", escape_pointer_token(part)))
        .collect()
}

fn escape_pointer_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("boolean literal is valid JSON")
}

fn string_projection(value: &str) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&serde_json::to_string(value).expect("string serialization"))
        .expect("serialized string is valid JSON")
}

fn empty_array() -> Utf16JsonProjection {
    Utf16JsonProjection::parse("[]").expect("empty array is valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> NativePolicyParseResult {
        parse_policy_document(raw, NativePolicyParseOptions::default())
    }

    #[test]
    fn source_oracle_available_models_filters_bad_members_and_keeps_siblings() {
        let result = parse(r#"{"availableModels":["opus",17,null],"model":"claude-sonnet-4-6"}"#);
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(normalized.string_at("/availableModels/0"), Some("opus"));
        assert_eq!(normalized.array_len("/availableModels"), Some(1));
        assert_eq!(normalized.string_at("/model"), Some("claude-sonnet-4-6"));
        assert_eq!(result.substituted_paths, Vec::<String>::new());
        assert!(result.diagnostics.iter().any(|item| {
            item.path == "/availableModels/1"
                && item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored
        }));
        assert!(result
            .diagnostics
            .iter()
            .any(|item| item.path == "/availableModels/2"));
    }

    #[test]
    fn source_oracle_available_models_wrong_root_falls_back_without_substitute_marker() {
        let result = parse(r#"{"availableModels":"opus"}"#);
        assert_eq!(
            result
                .normalized_projection
                .as_ref()
                .unwrap()
                .array_len("/availableModels"),
            Some(0)
        );
        assert!(result.substituted_paths.is_empty());
        assert!(!result.only_substitutes);
    }

    #[test]
    fn source_oracle_controls_ignore_invalid_and_null_without_rejecting_document() {
        let result = parse(
            r#"{"managedSourcesBehavior":null,"parentSettingsBehavior":"invalid","wslInheritsWindowsSettings":"true"}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert!(result.document_admitted);
        assert!(!normalized.contains_property("managedSourcesBehavior"));
        assert!(!normalized.contains_property("parentSettingsBehavior"));
        assert_eq!(
            normalized
                .at_pointer("/wslInheritsWindowsSettings")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(result.document_has_policy_content, Some(false));
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "/managedSourcesBehavior"
                && diagnostic.kind == NativePolicyDiagnosticKind::Ignored
        }));
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "/parentSettingsBehavior"
                && diagnostic.kind == NativePolicyDiagnosticKind::Ignored
        }));
        assert!(result.diagnostics.iter().any(|diagnostic| {
            diagnostic.path == "/wslInheritsWindowsSettings"
                && diagnostic.kind == NativePolicyDiagnosticKind::Coerced
                && diagnostic.status_only
        }));
    }

    #[test]
    fn source_oracle_behavior_controls_preserve_valid_enum_values_and_wsl_null_removal() {
        let result = parse(
            r#"{"managedSourcesBehavior":"merge","parentSettingsBehavior":"first-wins","wslInheritsWindowsSettings":null}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/managedSourcesBehavior"),
            Some("merge")
        );
        assert_eq!(
            normalized.string_at("/parentSettingsBehavior"),
            Some("first-wins")
        );
        assert!(!normalized.contains_property("wslInheritsWindowsSettings"));
        assert_eq!(result.document_has_policy_content, Some(true));
    }

    #[test]
    fn source_oracle_enforce_available_models_distinguishes_null_coercion_and_fallback() {
        let null = parse(r#"{"enforceAvailableModels":null}"#);
        assert!(!null
            .normalized_projection
            .as_ref()
            .unwrap()
            .contains_property("enforceAvailableModels"));
        assert!(null
            .diagnostics
            .iter()
            .any(|item| item.kind == NativePolicyDiagnosticKind::Removed));

        let coercion = parse(r#"{"enforceAvailableModels":"false"}"#);
        assert_eq!(
            coercion
                .normalized_projection
                .as_ref()
                .unwrap()
                .at_pointer("/enforceAvailableModels")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(coercion
            .diagnostics
            .iter()
            .any(|item| item.kind == NativePolicyDiagnosticKind::Coerced && !item.substituted));

        let invalid = parse(r#"{"enforceAvailableModels":"false-ish"}"#);
        assert_eq!(
            invalid
                .normalized_projection
                .as_ref()
                .unwrap()
                .at_pointer("/enforceAvailableModels")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(invalid.substituted_paths, vec!["/enforceAvailableModels"]);
        assert!(invalid.only_substitutes);
        assert!(invalid.diagnostics.iter().any(|item| {
            item.path == "/enforceAvailableModels"
                && item.kind == NativePolicyDiagnosticKind::OnlySubstitutes
                && item.status_only
                && item.only_substitutes
        }));
    }

    #[test]
    fn source_oracle_http_hook_allowlists_filter_entries_and_empty_invalid_fields() {
        let result = parse(
            r#"{"allowedHttpHookUrls":["https://ok/*",17],"httpHookAllowedEnvVars":[false]}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/allowedHttpHookUrls/0"),
            Some("https://ok/*")
        );
        assert_eq!(normalized.array_len("/allowedHttpHookUrls"), Some(1));
        assert_eq!(normalized.array_len("/httpHookAllowedEnvVars"), Some(0));
        assert!(result.substituted_paths.is_empty());
    }

    #[test]
    fn source_oracle_bad_sandbox_root_synthesizes_restrictive_shape_without_enabling_it() {
        let result = parse(r#"{"sandbox":"bad"}"#);
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/sandbox/autoAllowBashIfSandboxed")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(
            normalized
                .at_pointer("/sandbox/network/allowManagedDomainsOnly")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            normalized
                .at_pointer("/sandbox/network/strictAllowlist")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            normalized.string_at("/sandbox/credentials/sigv4/streaming"),
            Some("deny")
        );
        assert!(normalized.at_pointer("/sandbox/enabled").is_none());
        assert!(normalized
            .at_pointer("/sandbox/failIfUnavailable")
            .is_none());
        assert_eq!(result.synthesized_paths, vec!["/sandbox"]);
        assert!(result.only_substitutes);
    }

    #[test]
    fn source_oracle_sandbox_nested_object_fallback_preserves_valid_false_and_never_substitutes_fail_gate(
    ) {
        let result =
            parse(r#"{"sandbox":{"enabled":"false","failIfUnavailable":"bad","network":"bad"}}"#);
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/sandbox/enabled")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(normalized
            .at_pointer("/sandbox/failIfUnavailable")
            .is_none());
        assert_eq!(
            normalized
                .at_pointer("/sandbox/network/allowAllUnixSockets")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(result.synthesized_paths, vec!["/sandbox/network"]);
        assert!(!result.only_substitutes);
    }

    #[test]
    fn source_oracle_root_admission_and_unresolved_schema_are_separate() {
        let result = parse(r#"{"allowedProviders":[17]}"#);
        assert!(result.document_admitted);
        assert_eq!(result.status, NativePolicyParseStatus::ParsedObject);
        assert!(result
            .unresolved_fields
            .contains(&"allowedProviders".to_owned()));
        assert!(result.document_has_policy_content.is_none());

        let non_object = parse("[]");
        assert!(!non_object.document_admitted);
        assert_eq!(
            non_object.status,
            NativePolicyParseStatus::RejectedNonObjectRoot
        );
    }

    #[test]
    fn source_oracle_user_writable_prefilter_keeps_only_disabling_plugin_rows() {
        let result = parse_policy_document(
            r#"{"managedMcpServers":{"x":{}},"isolation":{"required":true},"prependPlugins":["x"],"enabledPlugins":{"user@local":false,"user2@local":true,"sec@builtin":false}}"#,
            NativePolicyParseOptions {
                user_writable: true,
            },
        );
        let candidate = result.candidate_projection.as_ref().unwrap();
        assert!(!candidate.contains_property("managedMcpServers"));
        assert!(!candidate.contains_property("isolation"));
        assert!(!candidate.contains_property("prependPlugins"));
        assert_eq!(
            candidate
                .at_pointer("/enabledPlugins/user@local")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(candidate
            .at_pointer("/enabledPlugins/user2@local")
            .is_none());
        assert!(candidate
            .at_pointer("/enabledPlugins/sec@builtin")
            .is_none());
    }

    #[test]
    fn source_oracle_raw_projection_survives_normalization_and_invalid_document() {
        let result = parse(r#"{"availableModels":["opus","\ud800",17]}"#);
        let raw = result.raw_projection.as_ref().unwrap();
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(raw.string_at("/availableModels/0"), Some("opus"));
        assert_eq!(raw.string_units("/availableModels/1"), Some(vec![0xd800]));
        assert_eq!(normalized.array_len("/availableModels"), Some(2));
        assert_eq!(
            normalized.string_units("/availableModels/1"),
            Some(vec![0xd800])
        );

        let invalid = parse("{broken");
        assert!(!invalid.document_admitted);
        assert!(invalid.raw_projection.is_none());
        assert!(invalid.normalized_projection.is_none());
        assert_eq!(invalid.raw_document, "{broken");
    }

    #[test]
    fn source_oracle_empty_available_models_is_content_but_not_only_substitutes() {
        let result = parse(r#"{"availableModels":[]}"#);
        assert_eq!(result.document_has_policy_content, Some(true));
        assert!(!result.only_substitutes);

        let invalid = parse(r#"{"availableModels":"not-an-array"}"#);
        assert_eq!(
            invalid
                .normalized_projection
                .as_ref()
                .unwrap()
                .array_len("/availableModels"),
            Some(0)
        );
        assert_eq!(invalid.document_has_policy_content, Some(true));
        assert!(!invalid.only_substitutes);
    }
}
