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
//! 280595. `enabledPlugins`' top-level schema is at 240238, its value schema
//! `va` at 206534, and the tolerant `os` field handler at 290913. `va` accepts
//! string arrays, booleans, and undefined (the latter is not representable in
//! JSON); its strings are not semver-parsed by this schema. `EP` is the
//! `ZodUndefined` helper in `src_186578912.js` (binary offset 186578912, SHA-256
//! `8886b2923ae669a979e16ab15d7a8151571d85efcd7471e0ba226d4e2d53559f`).
//! Policy-helper `I5e` record/refinement is in `src_187673145.js` at local
//! byte 204224; its `S()` startup-fatal wrapper is at 296660 and the
//! policyHelpers catch is at 302596. Zod's imported `PPs` input-type formatter
//! is byte-bound to the unique exporter `src_186567271.js` (base 186567271,
//! 11640 bytes, SHA-256
//! `5a4c66ea5dd1a86915555f0595322039f87aec3b7f58797233acbdb1515ae4b3`), whose
//! export list is imported by `src_186578912.js`; this parser mirrors its
//! JSON-created input types only. `fTr` at 371765 validates the full current
//! managed-settings schema as a separate later stage, still unresolved for
//! helper static payloads.
//! The tests below are fixture oracles for those source branches; they do not
//! execute Native.

use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Native managed-policy paths normalized by this module.
///
/// All other fields remain in the passthrough projection. A field can still be
/// listed in [`UNRESOLVED_NATIVE_POLICY_FIELDS`] when its Native-specific
/// validator or fallback is not implemented here.
pub const COVERED_NATIVE_POLICY_FIELDS: &[&str] = &[
    "availableModels",
    "availableModelsMatch",
    "deniedModels",
    "enforceAvailableModels",
    "allowedHttpHookUrls",
    "httpHookAllowedEnvVars",
    "managedSourcesBehavior",
    "parentSettingsBehavior",
    "wslInheritsWindowsSettings",
    "enabledPlugins",
    "prependPlugins",
    "appendPlugins",
    "allowedMcpServers",
    "deniedMcpServers",
    "strictPluginOnlyCustomization",
    "disableAllHooks",
    "modelProposedGoals",
    "feedbackDrafts",
    "askUserQuestionTimeout",
    "dialogExpiry",
    "crossSessionInbound",
    "allowedProviders",
    "allowedChannelPlugins",
    "strictKnownMarketplaces",
    "blockedMarketplaces",
    "allowedMarketplaces",
    "worktree.bgIsolation",
    "gatewayInternalNetworks",
    "forceLoginOrgUUID",
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
    "policyHelper",
    "policyHelpers",
    "permissions.disableBypassPermissionsMode",
    "permissions.disableAutoMode",
    "permissions.blockReadsOutsideWorkingDirectories",
    "autoMode.classifyAllShell",
    "disableDeepLinkRegistration",
];

/// Native managed-policy parsers with source-specific normalization not yet
/// represented by this module. Present fields are preserved, not claimed as
/// validated or normalized.
pub const UNRESOLVED_NATIVE_POLICY_FIELDS: &[&str] = &[
    "extraKnownMarketplaces",
    "managedMcpServers",
    "remoteTools.allowUnattendedServing",
    "attribution.sessionUrl",
    "remoteControl.shareHostProfile",
    "isolation.required",
    "isolation.persistHome",
];

/// Structural status for one policy document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePolicyParseStatus {
    /// The input was not valid JSON. The raw text is still retained.
    #[cfg(test)]
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
    /// A blocked marketplace entry was retained but Native cannot enforce
    /// its JavaScript regular-expression pattern.
    UnenforceableEntryKept,
    /// Native found no enforceable blocked-marketplace entries in a nonempty
    /// list, after reporting any invalid members.
    NoEnforceableBlockedEntries,
    /// An invalid array field was replaced with an empty fail-closed list.
    FallbackEmpty,
    /// A failed field parser used a non-empty Native fallback value.
    FallbackValue,
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

/// Exact source message for one policy-helper diagnostic marked
/// `startupFatal` by Native's `os()` parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyStartupFatalDiagnostic {
    /// Native's dotted policy field path.
    pub path: String,
    /// Exact Native load-error message when its validator message is
    /// represented. `None` preserves the unresolved source boundary without
    /// synthesizing Native-like prose.
    pub message: Option<Utf16JsonProjection>,
}

/// Exact message emitted by Native's policy-helper schema preprocessing or
/// catch path. Marketplace diagnostics deliberately use their own smaller
/// shared type; this carrier is limited to the helper parser's source text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyHelperDiagnostic {
    /// Native's dotted policy field path.
    pub path: String,
    /// Exact message text as a UTF-16 projection.
    pub message: Utf16JsonProjection,
    /// Native marks this message as status-only rather than an admission
    /// failure.
    pub status_only: bool,
}

/// Raw and normalized views of one managed-policy document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativePolicyParseResult {
    /// Exact raw UTF-8 document text, including text that fails JSON parsing.
    pub raw_document: String,
    /// Raw parsed JSON projection, when the input is valid JSON.
    pub raw_projection: Option<Utf16JsonProjection>,
    /// Projection supplied to the field-level policy normalizers. MDM
    /// `userWritable` is source metadata and is evaluated by the selector,
    /// not implemented as an input-field filter here.
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
    /// Policy-helper diagnostics that Native marks `startupFatal`.
    pub startup_fatal_paths: Vec<String>,
    /// Exact Native `startupFatal` messages when their issue formatting is
    /// source-represented. This is separate from ordinary parser diagnostics
    /// so those shared constructors remain independent of helper policy.
    pub startup_fatal_diagnostics: Vec<NativePolicyStartupFatalDiagnostic>,
    /// Exact policy-helper status and validation diagnostics, in Native
    /// preprocessing/schema traversal order.
    pub helper_diagnostics: Vec<NativePolicyHelperDiagnostic>,
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
#[cfg(test)]
pub fn parse_policy_document(raw: &str) -> NativePolicyParseResult {
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
                startup_fatal_paths: Vec::new(),
                startup_fatal_diagnostics: Vec::new(),
                helper_diagnostics: Vec::new(),
                unresolved_fields: Vec::new(),
            };
        }
    };
    parse_policy_projection_with_document(raw.to_owned(), raw_projection)
}

/// Normalize a projection that was already produced by the source JSON parser.
/// This deliberately does not stringify and reparse the projection: JavaScript
/// JSON.parse preserves overflow numbers as Infinity, while JSON.stringify
/// would turn them into null. `raw_document` is an audit/display copy only; all
/// validation reads `raw_projection` directly.
#[must_use]
pub fn parse_policy_projection(
    raw_document: String,
    raw_projection: Utf16JsonProjection,
) -> NativePolicyParseResult {
    parse_policy_projection_with_document(raw_document, raw_projection)
}

fn parse_policy_projection_with_document(
    raw_document: String,
    raw_projection: Utf16JsonProjection,
) -> NativePolicyParseResult {
    if raw_projection.object_entries().is_none() {
        return NativePolicyParseResult {
            raw_document: raw_document.clone(),
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
            startup_fatal_paths: Vec::new(),
            startup_fatal_diagnostics: Vec::new(),
            helper_diagnostics: Vec::new(),
            unresolved_fields: Vec::new(),
        };
    }

    let candidate_projection = raw_projection.clone();

    let mut normalized_projection = candidate_projection.clone();
    let mut diagnostics = Vec::new();
    let mut substituted_paths = Vec::new();
    let mut synthesized_paths = Vec::new();
    let mut dynamically_unresolved_fields = Vec::new();
    let mut startup_fatal_paths = Vec::new();
    let mut startup_fatal_diagnostics = Vec::new();
    let mut helper_diagnostics = Vec::new();

    normalize_marketplace_aliases(&mut normalized_projection, &mut diagnostics);
    normalize_native_policy_helpers(
        &mut normalized_projection,
        &mut diagnostics,
        &mut dynamically_unresolved_fields,
        &mut startup_fatal_paths,
        &mut startup_fatal_diagnostics,
        &mut helper_diagnostics,
    );
    normalize_enabled_plugins(&mut normalized_projection, &mut diagnostics);
    normalize_policy_marketplace_field(
        &mut normalized_projection,
        "strictKnownMarketplaces",
        &mut diagnostics,
        &mut dynamically_unresolved_fields,
    );
    normalize_policy_marketplace_field(
        &mut normalized_projection,
        "blockedMarketplaces",
        &mut diagnostics,
        &mut dynamically_unresolved_fields,
    );
    normalize_caught_string_array(
        &mut normalized_projection,
        "prependPlugins",
        &mut diagnostics,
    );
    normalize_caught_string_array(
        &mut normalized_projection,
        "appendPlugins",
        &mut diagnostics,
    );
    normalize_denied_models(&mut normalized_projection, &mut diagnostics);
    normalize_mcp_server_list(
        &mut normalized_projection,
        "allowedMcpServers",
        true,
        &mut diagnostics,
    );
    normalize_mcp_server_list(
        &mut normalized_projection,
        "deniedMcpServers",
        false,
        &mut diagnostics,
    );
    normalize_caught_enum(
        &mut normalized_projection,
        "availableModelsMatch",
        &["prefix", "exact"],
        &mut diagnostics,
    );
    normalize_strict_plugin_customization(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
    );
    normalize_caught_enum(
        &mut normalized_projection,
        "modelProposedGoals",
        &["auto", "alwaysAsk", "disabled"],
        &mut diagnostics,
    );
    normalize_caught_enum(
        &mut normalized_projection,
        "feedbackDrafts",
        &["notify", "quiet", "off"],
        &mut diagnostics,
    );
    normalize_caught_enum(
        &mut normalized_projection,
        "askUserQuestionTimeout",
        &["60s", "5m", "10m", "never"],
        &mut diagnostics,
    );
    normalize_caught_enum(
        &mut normalized_projection,
        "dialogExpiry",
        &["60s", "5m", "10m", "never"],
        &mut diagnostics,
    );
    normalize_caught_enum(
        &mut normalized_projection,
        "crossSessionInbound",
        &["accept", "hold", "refuse"],
        &mut diagnostics,
    );
    normalize_allowed_providers(&mut normalized_projection, &mut diagnostics);
    normalize_allowed_channel_plugins(&mut normalized_projection, &mut diagnostics);
    normalize_gateway_internal_networks(&mut normalized_projection, &mut diagnostics);
    normalize_force_login_org_uuid(&mut normalized_projection, &mut diagnostics);
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
    normalize_auto_mode(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
        &mut synthesized_paths,
    );
    normalize_worktree(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
        &mut synthesized_paths,
    );
    normalize_permission_locks(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
        &mut synthesized_paths,
    );
    normalize_bool_field(
        &mut normalized_projection,
        &["permissions", "blockReadsOutsideWorkingDirectories"],
        Some(true),
        &mut diagnostics,
        &mut substituted_paths,
    );
    normalize_sandbox(
        &mut normalized_projection,
        &mut diagnostics,
        &mut substituted_paths,
        &mut synthesized_paths,
    );

    let mut unresolved_fields = collect_unresolved_fields(&normalized_projection);
    for field in dynamically_unresolved_fields {
        if !unresolved_fields.contains(&field) {
            unresolved_fields.push(field);
        }
    }
    for field in &unresolved_fields {
        diagnostics.push(NativePolicyParseDiagnostic {
            path: format!("/{}", field.replace('.', "/")),
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
        raw_document: raw_document.clone(),
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
        startup_fatal_paths,
        startup_fatal_diagnostics,
        helper_diagnostics,
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

fn normalize_auto_mode(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
    synthesized_paths: &mut Vec<String>,
) {
    if !normalize_restricted_object_root(
        projection,
        "autoMode",
        Utf16JsonProjection::object(vec![("classifyAllShell".into(), bool_projection(true))])
            .expect("autoMode restrictive skeleton is valid JSON"),
        diagnostics,
        substituted_paths,
        synthesized_paths,
    ) {
        return;
    }
    normalize_bool_field(
        projection,
        &["autoMode", "classifyAllShell"],
        Some(true),
        diagnostics,
        substituted_paths,
    );
}

/// Normalize Native `Bo("worktree", ...)`: malformed strict siblings cause a
/// restrictive object skeleton, while `bgIsolation` itself substitutes to
/// `"worktree"` and the desktop-only `location` catch drops invalid values.
fn normalize_worktree(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
    synthesized_paths: &mut Vec<String>,
) {
    let skeleton =
        Utf16JsonProjection::object(vec![("bgIsolation".into(), string_projection("worktree"))])
            .expect("worktree restrictive skeleton is valid JSON");
    if !normalize_restricted_object_root(
        projection,
        "worktree",
        skeleton.clone(),
        diagnostics,
        substituted_paths,
        synthesized_paths,
    ) {
        return;
    }

    let Some(worktree) = projection.subprojection("/worktree").ok() else {
        return;
    };
    let mut fields = Vec::new();
    for field in ["symlinkDirectories", "sparsePaths"] {
        let Some(value) = worktree.subprojection(&format!("/{field}")).ok() else {
            continue;
        };
        let Some(value_root) = value.at_pointer("") else {
            continue;
        };
        if value_root.is_null() {
            continue;
        }
        let valid = value_root.array_len().is_some_and(|length| {
            (0..length).all(|index| {
                value_root
                    .get_index(index)
                    .and_then(JsonValueView::code_units)
                    .is_some()
            })
        });
        if !valid {
            set_path(projection, &["worktree"], skeleton);
            let path = "/worktree".to_owned();
            substituted_paths.push(path.clone());
            synthesized_paths.push(path.clone());
            diagnostics.push(diagnostic(
                path,
                NativePolicyDiagnosticKind::Synthesized,
                true,
                true,
                false,
            ));
            return;
        }
        fields.push((field.to_owned(), value));
    }

    if let Ok(value) = worktree.subprojection("/baseRef") {
        if let Some(value_root) = value.at_pointer("") {
            if !value_root.is_null() {
                let valid = value_root
                    .as_str()
                    .is_some_and(|value| matches!(value, "fresh" | "head"));
                if !valid {
                    set_path(projection, &["worktree"], skeleton);
                    let path = "/worktree".to_owned();
                    substituted_paths.push(path.clone());
                    synthesized_paths.push(path.clone());
                    diagnostics.push(diagnostic(
                        path,
                        NativePolicyDiagnosticKind::Synthesized,
                        true,
                        true,
                        false,
                    ));
                    return;
                }
                fields.push(("baseRef".into(), value));
            }
        }
    }

    if let Ok(value) = worktree.subprojection("/bgIsolation") {
        if let Some(value_root) = value.at_pointer("") {
            if !value_root.is_null() {
                if value_root
                    .as_str()
                    .is_some_and(|value| matches!(value, "worktree" | "none"))
                {
                    fields.push(("bgIsolation".into(), value));
                } else {
                    fields.push(("bgIsolation".into(), string_projection("worktree")));
                    let path = "/worktree/bgIsolation".to_owned();
                    substituted_paths.push(path.clone());
                    diagnostics.push(diagnostic(
                        path,
                        NativePolicyDiagnosticKind::Substituted,
                        true,
                        false,
                        false,
                    ));
                }
            }
        }
    }

    if let Ok(value) = worktree.subprojection("/location") {
        if value
            .at_pointer("")
            .and_then(JsonValueView::code_units)
            .is_some()
        {
            fields.push(("location".into(), value));
        } else {
            diagnostics.push(diagnostic(
                "/worktree/location".into(),
                NativePolicyDiagnosticKind::Ignored,
                false,
                false,
                false,
            ));
        }
    }

    if let Ok(normalized) = Utf16JsonProjection::object(fields) {
        let _ = projection.replace_pointer_projection("/worktree", normalized);
    }
}

/// Source `ka`: booleans pass through; arrays retain only the four known
/// customization surfaces; a failed union is replaced by `true`.
fn normalize_strict_plugin_customization(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
) {
    const FIELD: &str = "strictPluginOnlyCustomization";
    let pointer = format!("/{FIELD}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.is_null() {
        let _ = projection.remove_field(FIELD);
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
    if let Some(length) = root.array_len() {
        // Rebuild directly from the original array so the retained strings,
        // including their exact UTF-16 representation, are never re-encoded.
        let items = (0..length)
            .filter_map(|index| value.subprojection(&format!("/{index}")).ok())
            .filter(|item| {
                item.at_pointer("")
                    .and_then(JsonValueView::as_str)
                    .is_some_and(|surface| matches!(surface, "skills" | "agents" | "hooks" | "mcp"))
            })
            .collect::<Vec<_>>();
        if items.len() != length {
            let mut diagnostic = diagnostic(
                pointer.clone(),
                NativePolicyDiagnosticKind::Ignored,
                false,
                false,
                false,
            );
            diagnostic.status_only = true;
            diagnostics.push(diagnostic);
        }
        if let Ok(array) = Utf16JsonProjection::array(items) {
            let _ = projection.set_field(FIELD, array);
        }
        return;
    }

    let _ = projection.set_field(FIELD, bool_projection(true));
    substituted_paths.push(pointer.clone());
    diagnostics.push(diagnostic(
        pointer,
        NativePolicyDiagnosticKind::Substituted,
        true,
        false,
        false,
    ));
}

fn normalize_caught_enum(
    projection: &mut Utf16JsonProjection,
    field: &str,
    allowed: &[&str],
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    let pointer = format!("/{field}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let valid = value
        .at_pointer("")
        .and_then(JsonValueView::as_str)
        .is_some_and(|value| allowed.contains(&value));
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

/// Source `pn(..., Zd(Pt))`: a bad root becomes an empty allowlist, while
/// invalid provider entries are filtered individually.
fn normalize_allowed_providers(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    const FIELD: &str = "allowedProviders";
    let pointer = format!("/{FIELD}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    let Some(length) = root.array_len() else {
        let _ = projection.set_field(FIELD, empty_array());
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
        return;
    };

    let mut retained = Vec::new();
    for index in 0..length {
        let valid = root
            .get_index(index)
            .and_then(JsonValueView::as_str)
            .is_some_and(is_native_provider_name);
        if valid {
            if let Ok(item) = value.subprojection(&format!("/{index}")) {
                retained.push(item);
            }
        } else {
            let mut item = diagnostic(
                format!("{pointer}/{index}"),
                NativePolicyDiagnosticKind::InvalidEntryIgnored,
                false,
                false,
                false,
            );
            item.status_only = true;
            diagnostics.push(item);
        }
    }
    if length > 0 && retained.is_empty() {
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
    }
    if let Ok(array) = Utf16JsonProjection::array(retained) {
        let _ = projection.set_field(FIELD, array);
    }
}

fn is_native_provider_name(value: &str) -> bool {
    matches!(
        value,
        "anthropic"
            | "customEndpoint"
            | "bedrock"
            | "vertex"
            | "foundry"
            | "anthropicAws"
            | "mantle"
            | "gateway"
    ) || value.strip_prefix("anthropic").is_some_and(|suffix| {
        let mut characters = suffix.chars();
        characters
            .next()
            .is_some_and(|first| first.is_ascii_uppercase())
            && characters.all(|character| character.is_ascii_alphanumeric())
    })
}

/// Source `pn(..., em(), ...)`: keep valid channel plugin records, convert
/// legacy `plugin@marketplace` strings into the documented object form, and
/// drop invalid entries. A malformed list fails closed to an empty allowlist.
fn normalize_allowed_channel_plugins(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    const FIELD: &str = "allowedChannelPlugins";
    let pointer = format!("/{FIELD}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    let Some(length) = root.array_len() else {
        let _ = projection.set_field(FIELD, empty_array());
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
        return;
    };

    let mut retained = Vec::new();
    for index in 0..length {
        let item_pointer = format!("/{index}");
        let Some(item) = value.subprojection(&item_pointer).ok() else {
            continue;
        };
        let Some(item_root) = item.at_pointer("") else {
            continue;
        };
        if let Some(legacy_id) = item_root.as_str() {
            if let Some((plugin, marketplace)) = parse_channel_plugin_id(legacy_id) {
                let object = Utf16JsonProjection::object(vec![
                    ("marketplace".into(), string_projection(&marketplace)),
                    ("plugin".into(), string_projection(&plugin)),
                ])
                .expect("channel plugin object is valid JSON");
                retained.push(object);
                let mut entry_diagnostic = diagnostic(
                    format!("{pointer}{item_pointer}"),
                    NativePolicyDiagnosticKind::Coerced,
                    false,
                    false,
                    false,
                );
                entry_diagnostic.status_only = true;
                diagnostics.push(entry_diagnostic);
            } else {
                diagnostics.push(diagnostic(
                    format!("{pointer}{item_pointer}"),
                    NativePolicyDiagnosticKind::InvalidEntryIgnored,
                    false,
                    false,
                    false,
                ));
            }
            continue;
        }

        if item_root.object_entries().is_some() {
            let Some(marketplace) = item.subprojection("/marketplace").ok() else {
                diagnostics.push(diagnostic(
                    format!("{pointer}{item_pointer}"),
                    NativePolicyDiagnosticKind::InvalidEntryIgnored,
                    false,
                    false,
                    false,
                ));
                continue;
            };
            let Some(plugin) = item.subprojection("/plugin").ok() else {
                diagnostics.push(diagnostic(
                    format!("{pointer}{item_pointer}"),
                    NativePolicyDiagnosticKind::InvalidEntryIgnored,
                    false,
                    false,
                    false,
                ));
                continue;
            };
            let valid = marketplace
                .at_pointer("")
                .and_then(JsonValueView::code_units)
                .is_some()
                && plugin
                    .at_pointer("")
                    .and_then(JsonValueView::code_units)
                    .is_some();
            if valid {
                let object = Utf16JsonProjection::object(vec![
                    ("marketplace".into(), marketplace),
                    ("plugin".into(), plugin),
                ])
                .expect("channel plugin object is valid JSON");
                retained.push(object);
                continue;
            }
        }
        diagnostics.push(diagnostic(
            format!("{pointer}{item_pointer}"),
            NativePolicyDiagnosticKind::InvalidEntryIgnored,
            false,
            false,
            false,
        ));
    }

    if length > 0 && retained.is_empty() {
        diagnostics.push(diagnostic(
            pointer.clone(),
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
    }
    if let Ok(array) = Utf16JsonProjection::array(retained) {
        let _ = projection.set_field(FIELD, array);
    }
}

enum HelperEntryParse {
    Valid(Utf16JsonProjection),
    Invalid,
    InvalidWithNativeIssue(String),
    Removed,
    Unresolved {
        projection: Utf16JsonProjection,
        fields: Vec<String>,
    },
}

#[derive(Debug, Clone)]
struct HelperSchemaIssue {
    path: Vec<String>,
    message: String,
}

fn helper_diagnostic(
    diagnostics: &mut Vec<NativePolicyHelperDiagnostic>,
    path: impl Into<String>,
    message: impl AsRef<str>,
    status_only: bool,
) {
    diagnostics.push(NativePolicyHelperDiagnostic {
        path: path.into(),
        message: string_projection(message.as_ref()),
        status_only,
    });
}

fn format_helper_issues(issues: &[HelperSchemaIssue], limit: usize) -> Option<String> {
    let formatted = issues
        .iter()
        .take(limit)
        .map(|issue| {
            if issue.path.is_empty() {
                issue.message.clone()
            } else {
                format!("{}: {}", issue.path.join("."), issue.message)
            }
        })
        .collect::<Vec<_>>();
    (!formatted.is_empty()).then(|| formatted.join("; "))
}

fn helper_issue(path: &[&str], message: impl Into<String>) -> HelperSchemaIssue {
    HelperSchemaIssue {
        path: path.iter().map(|part| (*part).to_owned()).collect(),
        message: message.into(),
    }
}

fn helper_field_is_non_null(value: &Utf16JsonProjection, field: &str) -> bool {
    value
        .subprojection(&format!("/{field}"))
        .ok()
        .is_some_and(|field| field.at_pointer("").is_some_and(|field| !field.is_null()))
}

fn collect_singular_helper_status_diagnostics(
    value: &Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyHelperDiagnostic>,
) {
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.object_entries().is_none() {
        return;
    }
    let path = "policyHelper";
    for field in [
        "defaultSettings",
        "default",
        "managedSettings",
        "appendSystemPrompt",
    ] {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                path,
                format!(
                    "\"{field}\" on the singular policyHelper is ignored — static fallback payloads belong on the policyHelpers per-OS entries (\"defaultSettings\") or the map's \"default\" key. The intended fallback will NOT apply from here."
                ),
                true,
            );
        }
    }
    for field in ["policyHelper", "policyHelpers"] {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                path,
                format!(
                    "\"{field}\" inside the singular policyHelper is ignored — \"policyHelper\" and \"policyHelpers\" are TOP-LEVEL settings keys; nothing nests inside the singular entry. The nested config will NOT apply from here."
                ),
                true,
            );
        }
    }
    for field in POLICY_HELPER_OS_KEYS {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                path,
                format!(
                    "\"{field}\" on the singular policyHelper is ignored — per-OS entries live on the policyHelpers MAP (\"policyHelpers\": {{\"{field}\": ...}}), not inside the singular key. The intended per-OS config will NOT apply from here."
                ),
                true,
            );
        }
    }
    if helper_field_is_non_null(value, "claudeMd") {
        helper_diagnostic(
            diagnostics,
            path,
            "\"claudeMd\" on the singular policyHelper is ignored — \"claudeMd\" is a managed-settings key: put it at the settings top level or inside a static payload, or emit it from the helper's stdout envelope. The intended instructions will NOT apply from here.",
            true,
        );
    }
    for field in ["outputBehavior", "onFailure", "retries"] {
        if !helper_field_is_non_null(value, field) {
            continue;
        }
        let message = match field {
            "outputBehavior" => format!(
                "\"outputBehavior\" on the singular policyHelper is ignored — it is only honored on the policyHelpers per-OS entries (policyHelpers.{}); this helper's output REPLACES the policy tier whatever the value says.",
                POLICY_HELPER_OS_KEYS.join("/")
            ),
            "onFailure" => format!(
                "\"onFailure\" on the singular policyHelper is ignored — it is only honored on the policyHelpers per-OS entries (policyHelpers.{}); a failure of this helper REFUSES to start Claude Code whatever the value says.",
                POLICY_HELPER_OS_KEYS.join("/")
            ),
            _ => format!(
                "\"retries\" on the singular policyHelper is ignored — it is only honored on the policyHelpers per-OS entries (policyHelpers.{}); this helper is run ONCE per start or refresh, never re-run, whatever the value says.",
                POLICY_HELPER_OS_KEYS.join("/")
            ),
        };
        helper_diagnostic(diagnostics, path, message, true);
    }
}

fn collect_policy_helper_map_status_diagnostics(
    value: &Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyHelperDiagnostic>,
) {
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.object_entries().is_none() {
        return;
    }
    for field in [
        "defaultSettings",
        "managedSettings",
        "appendSystemPrompt",
        "claudeMd",
    ] {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                "policyHelpers",
                format!(
                    "\"{field}\" directly on the policyHelpers map is ignored — static fallback payloads go on a per-OS entry's \"defaultSettings\" or the map's \"default\" key (a managed-settings object), and helper-output keys come from the helper's stdout. The intended content will NOT apply from here."
                ),
                true,
            );
        }
    }
    for field in ["policyHelper", "policyHelpers"] {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                "policyHelpers",
                format!(
                    "\"{field}\" inside the policyHelpers map is ignored — \"policyHelper\" and \"policyHelpers\" are TOP-LEVEL settings keys; the map's keys are the per-OS entries and \"default\". The nested config will NOT apply from here."
                ),
                true,
            );
        }
    }
    for field in STATIC_HELPER_ENTRY_FIELDS
        .iter()
        .copied()
        .filter(|field| *field != "defaultSettings")
    {
        if !helper_field_is_non_null(value, field) {
            continue;
        }
        let suffix = match field {
            "script" | "interpreter" => {
                "; inline scripts are per-OS only (the singular policyHelper key takes a path)"
            }
            "outputBehavior" | "onFailure" | "retries" => "",
            _ => ", or on the singular policyHelper key",
        };
        helper_diagnostic(
            diagnostics,
            "policyHelpers",
            format!(
                "\"{field}\" directly on the policyHelpers map is ignored — helper configs go on a per-OS entry (policyHelpers.{}){suffix}. No helper runs from this field here.",
                POLICY_HELPER_OS_KEYS.join("/")
            ),
            true,
        );
    }
}

fn collect_per_os_helper_status_diagnostics(
    value: &Utf16JsonProjection,
    key: &str,
    diagnostics: &mut Vec<NativePolicyHelperDiagnostic>,
) {
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.object_entries().is_none() || key == "default" {
        return;
    }
    let path = format!("policyHelpers.{key}");
    for field in ["managedSettings", "appendSystemPrompt", "claudeMd"] {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                &path,
                format!(
                    "\"{field}\" on the {path} entry is ignored — helper output cannot be pre-seeded on an entry; a static fallback payload goes under this entry's \"defaultSettings\" (a managed-settings object). The intended content will NOT apply from here."
                ),
                true,
            );
        }
    }
    for field in ["policyHelper", "policyHelpers"] {
        if helper_field_is_non_null(value, field) {
            helper_diagnostic(
                diagnostics,
                &path,
                format!(
                    "\"{field}\" on the {path} entry is ignored — \"policyHelper\" and \"policyHelpers\" are TOP-LEVEL settings keys; nothing nests inside an entry. The nested config will NOT apply from here."
                ),
                true,
            );
        }
    }
    for os in POLICY_HELPER_OS_KEYS {
        if helper_field_is_non_null(value, os) {
            helper_diagnostic(
                diagnostics,
                &path,
                format!(
                    "\"{os}\" on the {path} entry is ignored — per-OS entries are SIBLINGS on the policyHelpers map, not nested inside each other. The intended {os} config will NOT apply from here."
                ),
                true,
            );
        }
    }
    if helper_field_is_non_null(value, "default") {
        helper_diagnostic(
            diagnostics,
            &path,
            format!(
                "\"default\" on the {path} entry is ignored — the per-entry static payload field is spelled \"defaultSettings\"; \"default\" is the MAP's any-platform catch-all key (a sibling of the OS entries). The intended fallback will NOT apply from here."
            ),
            true,
        );
    }
    let invalid_on_failure = value.subprojection("/onFailure").ok().and_then(|value| {
        let on_failure = value.at_pointer("")?;
        (!on_failure.is_null() && !matches!(on_failure.as_str(), Some("continue" | "refuse")))
            .then(|| on_failure_status_value(on_failure))
    });
    if let Some(rendered) = invalid_on_failure {
        helper_diagnostic(
            diagnostics,
            format!("{path}.onFailure"),
            format!(
                "\"onFailure\": {rendered} on the {path} entry is not a recognized value (\"continue\" or \"refuse\"); it is treated as \"refuse\" — a startup failure of this entry's helper with no static payload in its place will not start Claude Code."
            ),
            true,
        );
    }
}

fn on_failure_status_value(value: JsonValueView<'_>) -> String {
    if let Some(units) = value.code_units() {
        let sanitized = std::char::decode_utf16(units.iter().copied())
            .map(|decoded| decoded.unwrap_or('?'))
            .map(|character| {
                if character.is_ascii() && !character.is_ascii_control() {
                    character
                } else {
                    '?'
                }
            })
            .collect::<String>();
        let sanitized = if sanitized.encode_utf16().count() > 64 {
            let prefix = sanitized.chars().take(63).collect::<String>();
            format!("{prefix}…")
        } else {
            sanitized
        };
        return serde_json::to_string(&sanitized).expect("sanitized helper string serializes");
    }
    if let Some(value) = value.as_number() {
        return js_number_string(value);
    }
    if let Some(value) = value.as_bool() {
        return value.to_string();
    }
    if value.array_len().is_some() {
        "a non-string value (array)".to_owned()
    } else {
        "a non-string value (object)".to_owned()
    }
}

fn js_number_string(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_owned()
    } else if value == f64::INFINITY {
        "Infinity".to_owned()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_owned()
    } else if value == 0.0 {
        "0".to_owned()
    } else {
        let raw = serde_json::to_string(&value).unwrap_or_else(|_| value.to_string());
        let Some((mantissa, exponent)) = raw.split_once('e') else {
            return raw.strip_suffix(".0").unwrap_or(&raw).to_owned();
        };
        let Ok(exponent) = exponent.parse::<i32>() else {
            return raw;
        };
        let negative = mantissa.starts_with('-');
        let unsigned = mantissa.strip_prefix('-').unwrap_or(mantissa);
        let decimal_at = unsigned.find('.').unwrap_or(unsigned.len()) as i32;
        let digits = unsigned.replace('.', "");
        let point = decimal_at + exponent;
        let sign = if negative { "-" } else { "" };
        if (-6..21).contains(&exponent) {
            if point <= 0 {
                format!("{sign}0.{}{}", "0".repeat((-point) as usize), digits)
            } else if point as usize >= digits.len() {
                format!(
                    "{sign}{digits}{}",
                    "0".repeat(point as usize - digits.len())
                )
            } else {
                let point = point as usize;
                format!("{sign}{}.{}", &digits[..point], &digits[point..])
            }
        } else {
            let mut chars = digits.chars();
            let first = chars.next().unwrap_or('0');
            let rest = chars.collect::<String>();
            let sign_exp = if exponent >= 0 { "+" } else { "" };
            if rest.is_empty() {
                format!("{sign}{first}e{sign_exp}{exponent}")
            } else {
                format!("{sign}{first}.{rest}e{sign_exp}{exponent}")
            }
        }
    }
}

/// Normalize the two policy-helper source fields without routing their exact
/// strings through serde. Static settings payloads remain explicitly
/// unresolved until the complete Native `I5e` settings schema is represented.
fn normalize_native_policy_helpers(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    unresolved_fields: &mut Vec<String>,
    startup_fatal_paths: &mut Vec<String>,
    startup_fatal_diagnostics: &mut Vec<NativePolicyStartupFatalDiagnostic>,
    helper_diagnostics: &mut Vec<NativePolicyHelperDiagnostic>,
) {
    if let Ok(value) = projection.subprojection("/policyHelper") {
        collect_singular_helper_status_diagnostics(&value, helper_diagnostics);
        let parsed_entry = normalize_singular_policy_helper(&value);
        let native_issue = match &parsed_entry {
            HelperEntryParse::InvalidWithNativeIssue(issue) => Some(issue.clone()),
            _ => None,
        };
        match parsed_entry {
            HelperEntryParse::Valid(normalized) => {
                let _ = projection.set_field("policyHelper", normalized);
            }
            HelperEntryParse::Invalid | HelperEntryParse::InvalidWithNativeIssue(_) => {
                let path_is_absent_or_null = match value.subprojection("/path") {
                    Ok(path) => path.at_pointer("").is_none_or(JsonValueView::is_null),
                    Err(_) => true,
                };
                let message = native_issue
                    .or_else(|| singular_helper_error(&value))
                    .unwrap_or_else(|| "Failed schema validation".to_owned());
                helper_diagnostic(
                    helper_diagnostics,
                    "policyHelper",
                    format!("{message}. This field was ignored."),
                    path_is_absent_or_null,
                );
                let _ = projection.remove_field("policyHelper");
                diagnostics.push(diagnostic(
                    "/policyHelper".into(),
                    NativePolicyDiagnosticKind::Ignored,
                    false,
                    false,
                    false,
                ));
            }
            HelperEntryParse::Removed => {
                let _ = projection.remove_field("policyHelper");
            }
            HelperEntryParse::Unresolved { fields, .. } => {
                for field in fields {
                    push_unresolved_field(unresolved_fields, &field);
                }
            }
        }
    }

    let Ok(source_map) = projection.subprojection("/policyHelpers") else {
        return;
    };
    let Some(map_root) = source_map.at_pointer("") else {
        push_unresolved_field(unresolved_fields, "policyHelpers");
        return;
    };
    if map_root.is_null() {
        // Native's preprocessing schema treats explicit null as key removal
        // before the object schema runs.
        let _ = projection.remove_field("policyHelpers");
        return;
    }
    if map_root.object_entries().is_none() {
        let _ = projection.remove_field("policyHelpers");
        let message = invalid_policy_helpers_root_message(&source_map);
        if let Some(message) = message.as_ref() {
            helper_diagnostic(helper_diagnostics, "policyHelpers", message, false);
        }
        push_startup_fatal_diagnostic(
            startup_fatal_paths,
            startup_fatal_diagnostics,
            "policyHelpers",
            message.map(|message| string_projection(&message)),
        );
        diagnostics.push(diagnostic(
            "/policyHelpers".into(),
            NativePolicyDiagnosticKind::Ignored,
            false,
            false,
            false,
        ));
        return;
    }

    collect_policy_helper_map_status_diagnostics(&source_map, helper_diagnostics);

    let mut normalized_entries = Vec::new();
    // Native's Zod object is constructed from `Zo.map(...)`, so its property
    // parser and output order follow the schema keys, not input object order.
    for key in POLICY_HELPER_OS_KEYS.iter().copied().chain(["default"]) {
        let Ok(value) = source_map.subprojection(&format!("/{key}")) else {
            continue;
        };
        if key == "default" {
            if value.at_pointer("").is_some_and(JsonValueView::is_null) {
                continue;
            }
            if !passes_native_i5e_static_helper_schema(&value) {
                diagnostics.push(diagnostic(
                    "/policyHelpers/default".into(),
                    NativePolicyDiagnosticKind::Ignored,
                    false,
                    false,
                    false,
                ));
                let message = static_helper_settings_issue(&value).map(|issue| {
                    string_projection(&static_helper_startup_fatal_message(
                        "policyHelpers.default",
                        &issue,
                    ))
                });
                if let Some(message) = message.as_ref() {
                    helper_diagnostic(
                        helper_diagnostics,
                        "policyHelpers.default",
                        message.string_at("").unwrap_or(""),
                        false,
                    );
                }
                push_startup_fatal_diagnostic(
                    startup_fatal_paths,
                    startup_fatal_diagnostics,
                    "policyHelpers.default",
                    message,
                );
                continue;
            }
            normalized_entries.push((key.to_owned(), value));
            continue;
        }
        let path = format!("/policyHelpers/{key}");
        let invalid_static_fallback = value
            .subprojection("/defaultSettings")
            .ok()
            .filter(|settings| !settings.at_pointer("").is_some_and(JsonValueView::is_null))
            .is_some_and(|settings| !passes_native_i5e_static_helper_schema(&settings));
        if invalid_static_fallback {
            let fallback_issue = value
                .subprojection("/defaultSettings")
                .ok()
                .and_then(|settings| static_helper_settings_issue(&settings));
            let path = format!("policyHelpers.{key}.defaultSettings");
            let message = fallback_issue.map(|issue| {
                string_projection(&static_helper_startup_fatal_message(&path, &issue))
            });
            if let Some(message) = message.as_ref() {
                helper_diagnostic(
                    helper_diagnostics,
                    &path,
                    message.string_at("").unwrap_or(""),
                    false,
                );
            }
            push_startup_fatal_diagnostic(
                startup_fatal_paths,
                startup_fatal_diagnostics,
                &path,
                message,
            );
        }
        collect_per_os_helper_status_diagnostics(&value, &key, helper_diagnostics);
        let parsed_entry = normalize_per_os_helper_entry(&value, &key);
        let parsed_entry_issue = match &parsed_entry {
            HelperEntryParse::InvalidWithNativeIssue(issue) => Some(issue.clone()),
            _ => None,
        };
        match parsed_entry {
            HelperEntryParse::Valid(normalized) => {
                normalized_entries.push((key.to_owned(), normalized));
            }
            HelperEntryParse::Invalid | HelperEntryParse::InvalidWithNativeIssue(_) => {
                diagnostics.push(diagnostic(
                    path,
                    NativePolicyDiagnosticKind::Ignored,
                    false,
                    false,
                    false,
                ));
                if !invalid_static_fallback {
                    let entry_issue = parsed_entry_issue.clone().or_else(|| {
                        format_helper_issues(&per_os_helper_schema_issues(&value, &key), 3)
                    });
                    if let Some(message) = per_os_helper_catch_message(
                        &source_map,
                        &key,
                        &value,
                        entry_issue.as_deref().unwrap_or("failed validation"),
                    ) {
                        helper_diagnostic(
                            helper_diagnostics,
                            format!("policyHelpers.{key}"),
                            &message,
                            !invalid_entry_requires_startup_fatal(&value)
                                || has_static_fallback_for_policy_entry(&source_map, &key),
                        );
                    }
                    if invalid_entry_requires_startup_fatal(&value)
                        && !has_static_fallback_for_policy_entry(&source_map, &key)
                    {
                        let message = entry_issue.map(|issue| {
                            string_projection(&format!(
                                "{issue} Its \"onFailure\" requires the helper, so when delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed."
                            ))
                        });
                        push_startup_fatal_diagnostic(
                            startup_fatal_paths,
                            startup_fatal_diagnostics,
                            &format!("policyHelpers.{key}"),
                            message,
                        );
                    }
                }
                if !invalid_static_fallback {
                    if let Some(fallback) = normalize_static_fallback_only(&value) {
                        // I5e validates this rescued payload here; the Core
                        // collector separately runs the complete At/Due chain.
                        normalized_entries.push((key.to_owned(), fallback));
                    }
                }
            }
            HelperEntryParse::Removed => {}
            HelperEntryParse::Unresolved { fields, projection } => {
                for field in fields {
                    push_unresolved_field(unresolved_fields, &field);
                }
                normalized_entries.push((key.to_owned(), projection));
            }
        }
    }

    let normalized_map = Utf16JsonProjection::object(normalized_entries)
        .expect("known helper map keys and projections form a JSON object");
    let _ = projection.set_field("policyHelpers", normalized_map);
}

fn normalize_singular_policy_helper(value: &Utf16JsonProjection) -> HelperEntryParse {
    let Some(root) = value.at_pointer("") else {
        return HelperEntryParse::Invalid;
    };
    if root.is_null() {
        return HelperEntryParse::Removed;
    }
    if root.object_entries().is_none() {
        return HelperEntryParse::Invalid;
    }
    let Some(path) = value.subprojection("/path").ok() else {
        return HelperEntryParse::Invalid;
    };
    let Some(path_root) = path.at_pointer("") else {
        return HelperEntryParse::Invalid;
    };
    if path_root.code_units().is_none() {
        return HelperEntryParse::Invalid;
    }

    let timeout = match optional_helper_integer(value, "timeoutMs", 1_000, false) {
        Ok(value) => value,
        Err(()) => return HelperEntryParse::Invalid,
    };
    let refresh = match optional_helper_integer(value, "refreshIntervalMs", 60_000, true) {
        Ok(value) => value,
        Err(()) => return HelperEntryParse::Invalid,
    };
    let mut fields = vec![("path".to_owned(), path)];
    if let Some(timeout) = timeout {
        fields.push(("timeoutMs".into(), timeout));
    }
    if let Some(refresh) = refresh {
        fields.push(("refreshIntervalMs".into(), refresh));
    }
    HelperEntryParse::Valid(
        Utf16JsonProjection::object(fields).expect("validated helper config is an object"),
    )
}

fn normalize_per_os_helper_entry(value: &Utf16JsonProjection, os: &str) -> HelperEntryParse {
    let Some(root) = value.at_pointer("") else {
        return HelperEntryParse::Invalid;
    };
    if root.is_null() {
        return HelperEntryParse::Removed;
    }
    if root.object_entries().is_none() {
        return HelperEntryParse::Invalid;
    }

    if helper_entry_is_null_tombstone(value) {
        return HelperEntryParse::Removed;
    }

    let mut fields = Vec::new();
    let path = match optional_helper_string(value, "path") {
        Ok(None) => None,
        Ok(Some(path)) => match validate_helper_path(&path, os) {
            HelperPathValidation::Valid => Some(path),
            HelperPathValidation::Invalid => return HelperEntryParse::Invalid,
            HelperPathValidation::Unresolved => {
                return HelperEntryParse::Unresolved {
                    projection: value.clone(),
                    fields: vec![format!("policyHelpers.{os}.path")],
                };
            }
        },
        Err(()) => return HelperEntryParse::Invalid,
    };
    let script = match optional_helper_string(value, "script") {
        Ok(None) => None,
        Ok(Some(script)) if valid_helper_script(&script, os) => Some(script),
        _ => return HelperEntryParse::Invalid,
    };
    let interpreter = match optional_helper_string(value, "interpreter") {
        Ok(None) => None,
        Ok(Some(interpreter))
            if (os == "windows" && utf16_is_ascii(&interpreter, b"pwsh"))
                || (os != "windows" && utf16_is_ascii(&interpreter, b"sh")) =>
        {
            Some(interpreter)
        }
        _ => return HelperEntryParse::Invalid,
    };
    let output_behavior = match optional_helper_string(value, "outputBehavior") {
        Ok(None) => None,
        Ok(Some(output))
            if utf16_is_ascii(&output, b"replace") || utf16_is_ascii(&output, b"merge") =>
        {
            Some(output)
        }
        _ => return HelperEntryParse::Invalid,
    };
    let on_failure = match optional_helper_string(value, "onFailure") {
        Ok(None) => None,
        Ok(Some(on_failure))
            if utf16_is_ascii(&on_failure, b"continue")
                || utf16_is_ascii(&on_failure, b"refuse") =>
        {
            Some(on_failure)
        }
        Ok(Some(_)) => Some(string_projection("refuse")),
        Err(()) => Some(string_projection("refuse")),
    };
    let retries = match optional_helper_integer(value, "retries", 0, false) {
        Ok(Some(retries)) => Some(clamp_integer_projection(retries, 5)),
        Ok(None) => None,
        Err(()) => return HelperEntryParse::Invalid,
    };
    let timeout = match optional_helper_integer(value, "timeoutMs", 1_000, false) {
        Ok(timeout) => timeout.map(|value| clamp_integer_projection(value, 2_147_483_647)),
        Err(()) => return HelperEntryParse::Invalid,
    };
    let refresh = match optional_helper_integer(value, "refreshIntervalMs", 60_000, true) {
        Ok(refresh) => refresh.map(|value| clamp_integer_projection(value, 2_147_483_647)),
        Err(()) => return HelperEntryParse::Invalid,
    };
    let default_settings = match value.subprojection("/defaultSettings") {
        Ok(settings) if settings.at_pointer("").is_some_and(JsonValueView::is_null) => None,
        Ok(settings) if passes_native_i5e_static_helper_schema(&settings) => Some(settings),
        // The caller records the source-backed startup-fatal path and strips
        // this invalid fallback while retaining any valid helper config.
        Ok(_) => None,
        Err(_) => None,
    };

    if path.is_some() && script.is_some() {
        return HelperEntryParse::InvalidWithNativeIssue(
            "exactly one of path/script must be configured".into(),
        );
    }
    if script.is_some() != interpreter.is_some() {
        return HelperEntryParse::InvalidWithNativeIssue(
            "\"script\" and \"interpreter\" must be configured together".into(),
        );
    }
    if path.is_none() && script.is_none() && default_settings.is_none() {
        return HelperEntryParse::InvalidWithNativeIssue(
            "Entry must carry \"path\" (a helper executable) or \"script\" + \"interpreter\" (an inline helper), and/or \"defaultSettings\" (a static settings payload)".into(),
        );
    }

    for (field, value) in [
        ("timeoutMs", timeout),
        ("refreshIntervalMs", refresh),
        ("path", path),
        ("script", script),
        ("interpreter", interpreter),
        ("outputBehavior", output_behavior),
        ("onFailure", on_failure),
        ("retries", retries),
        ("defaultSettings", default_settings),
    ] {
        if let Some(value) = value {
            fields.push((field.to_owned(), value));
        }
    }
    let projection =
        Utf16JsonProjection::object(fields).expect("validated helper config is an object");
    HelperEntryParse::Valid(projection)
}

fn singular_helper_error(value: &Utf16JsonProjection) -> Option<String> {
    let root = value.at_pointer("")?;
    if root.object_entries().is_none() {
        return Some(native_invalid_type_message("object", root));
    }
    let mut issues = Vec::new();
    let path = value.subprojection("/path").ok();
    match path.as_ref().and_then(|field| field.at_pointer("")) {
        Some(path) if path.code_units().is_some() => {}
        input => issues.push(helper_issue(
            &["path"],
            native_invalid_type_message_for_option("string", input),
        )),
    }
    for (field, minimum) in [("timeoutMs", 1_000), ("refreshIntervalMs", 60_000)] {
        let Some(field_projection) = value.subprojection(&format!("/{field}")).ok() else {
            continue;
        };
        if field_projection
            .at_pointer("")
            .is_some_and(JsonValueView::is_null)
        {
            continue;
        }
        let field_issues = if field == "refreshIntervalMs" {
            helper_refresh_interval_issues(&field_projection)
        } else {
            helper_integer_issues(&field_projection, minimum, 2_147_483_647)
        };
        issues.extend(field_issues.into_iter().map(|issue| HelperSchemaIssue {
            path: vec![field.to_owned()],
            message: issue,
        }));
    }
    issues.first().map(|issue| issue.message.clone())
}

/// Native `wa()` is built from `mn().omit({path:true}).extend(...)`. Object
/// shape order is therefore the remaining `mn` fields first, followed by the
/// extension fields: timeoutMs, refreshIntervalMs, path, script, interpreter,
/// outputBehavior, onFailure, retries, defaultSettings. Zod's object parser
/// uses that key order when `h()` takes its first three issues.
fn per_os_helper_schema_issues(value: &Utf16JsonProjection, os: &str) -> Vec<HelperSchemaIssue> {
    let Some(root) = value.at_pointer("") else {
        return Vec::new();
    };
    if helper_entry_is_null_tombstone(value) {
        return Vec::new();
    }
    if root.object_entries().is_none() {
        return vec![helper_issue(
            &[],
            native_invalid_type_message("object", root),
        )];
    }

    let mut issues = Vec::new();
    for (field, minimum) in [("timeoutMs", 1_000), ("refreshIntervalMs", 60_000)] {
        let Some(field_projection) = value.subprojection(&format!("/{field}")).ok() else {
            continue;
        };
        if field_projection
            .at_pointer("")
            .is_some_and(JsonValueView::is_null)
        {
            continue;
        }
        let field_issues = if field == "refreshIntervalMs" {
            helper_refresh_interval_issues(&field_projection)
        } else {
            helper_integer_issues(&field_projection, minimum, 2_147_483_647)
        };
        issues.extend(
            field_issues
                .into_iter()
                .map(|message| helper_issue(&[field], message)),
        );
    }

    let path = value.subprojection("/path").ok();
    let path_value = path.as_ref().and_then(|field| field.at_pointer(""));
    let path_is_present = match path_value {
        Some(path) if path.is_null() => false,
        Some(path) if path.code_units().is_some() => {
            // UTF-16 strings with an unpaired unit are still strings. The
            // path validators preserve their units rather than using the
            // projection's lossy display form.
            if let Some(units) = path.code_units() {
                issues.extend(
                    helper_path_unit_issues(units, os)
                        .into_iter()
                        .map(|message| helper_issue(&["path"], message)),
                );
            }
            true
        }
        Some(path) => {
            issues.push(helper_issue(
                &["path"],
                native_invalid_type_message("string", path),
            ));
            false
        }
        None => {
            issues.push(helper_issue(
                &["path"],
                native_invalid_type_message_for_option("string", None),
            ));
            false
        }
    };

    let script = value.subprojection("/script").ok();
    let script_value = script.as_ref().and_then(|field| field.at_pointer(""));
    let script_is_present = match script_value {
        Some(script) if script.is_null() => false,
        Some(script) if script.code_units().is_some() => {
            if let Some(units) = script.code_units() {
                issues.extend(
                    helper_script_issues(units, os)
                        .into_iter()
                        .map(|message| helper_issue(&["script"], message)),
                );
            }
            true
        }
        Some(script) => {
            issues.push(helper_issue(
                &["script"],
                native_invalid_type_message("string", script),
            ));
            false
        }
        None => false,
    };

    let interpreter = value.subprojection("/interpreter").ok();
    let interpreter_value = interpreter.as_ref().and_then(|field| field.at_pointer(""));
    let interpreter_is_present = match interpreter_value {
        Some(interpreter) if interpreter.is_null() => false,
        Some(interpreter) if interpreter.code_units().is_some() => {
            let expected = if os == "windows" { "pwsh" } else { "sh" };
            if !interpreter
                .as_str()
                .is_some_and(|actual| actual == expected)
            {
                let message = if os == "windows" {
                    "interpreter must be 'pwsh' on windows ('sh' is not supported there)"
                } else {
                    "interpreter must be 'sh' on macos/linux/wsl"
                };
                issues.push(helper_issue(&["interpreter"], message));
            }
            true
        }
        Some(_interpreter) => {
            let message = if os == "windows" {
                "interpreter must be 'pwsh' on windows ('sh' is not supported there)"
            } else {
                "interpreter must be 'sh' on macos/linux/wsl"
            };
            issues.push(helper_issue(&["interpreter"], message));
            false
        }
        None => false,
    };

    if let Ok(output) = value.subprojection("/outputBehavior") {
        if !output.at_pointer("").is_some_and(JsonValueView::is_null)
            && !matches!(output.string_at(""), Some("replace" | "merge"))
        {
            issues.push(helper_issue(
                &["outputBehavior"],
                invalid_option_message(&["replace", "merge"]),
            ));
        }
    }

    if let Ok(retries) = value.subprojection("/retries") {
        if !retries.at_pointer("").is_some_and(JsonValueView::is_null) {
            issues.extend(
                helper_integer_issues(&retries, 0, 5)
                    .into_iter()
                    .map(|message| helper_issue(&["retries"], message)),
            );
        }
    }

    let has_default_settings =
        value
            .subprojection("/defaultSettings")
            .ok()
            .is_some_and(|settings| {
                settings
                    .at_pointer("")
                    .is_some_and(|settings| !settings.is_null())
            });
    if issues.is_empty() {
        if path_is_present && script_is_present {
            issues.push(helper_issue(
                &[],
                "exactly one of path/script must be configured",
            ));
        } else if script_is_present != interpreter_is_present {
            issues.push(helper_issue(
                &[],
                "\"script\" and \"interpreter\" must be configured together",
            ));
        } else if !path_is_present && !script_is_present && !has_default_settings {
            issues.push(helper_issue(
                &[],
                "Entry must carry \"path\" (a helper executable) or \"script\" + \"interpreter\" (an inline helper), and/or \"defaultSettings\" (a static settings payload)",
            ));
        }
    }
    issues
}

fn helper_integer_issues(value: &Utf16JsonProjection, minimum: u32, _maximum: u32) -> Vec<String> {
    let Some(root) = value.at_pointer("") else {
        return vec!["Invalid input: expected number, received undefined".into()];
    };
    let Some(number) = root.as_number() else {
        return vec![native_invalid_type_message("number", root)];
    };
    if !number.is_finite() {
        return vec![format!(
            "Invalid input: expected number, received {}",
            js_number_string(number)
        )];
    }
    if number.fract() != 0.0 {
        return vec!["Invalid input: expected int, received number".into()];
    }
    if number.abs() > 9_007_199_254_740_991.0 {
        if number > 0.0 {
            return vec![format!("Too big: expected int to be <=9007199254740991")];
        }
        return vec!["Too small: expected int to be >=-9007199254740991".into()];
    }
    if number < f64::from(minimum) {
        return vec![format!("Too small: expected number to be >= {}", minimum)];
    }
    Vec::new()
}

fn helper_refresh_interval_issues(value: &Utf16JsonProjection) -> Vec<String> {
    let Some(root) = value.at_pointer("") else {
        return vec!["Invalid input".into()];
    };
    if root.as_number() == Some(0.0) {
        return Vec::new();
    }
    if helper_integer_issues(value, 60_000, 2_147_483_647).is_empty() {
        Vec::new()
    } else {
        vec!["Invalid input".into()]
    }
}

fn helper_script_issues(units: &[u16], os: &str) -> Vec<String> {
    let mut issues = Vec::new();
    if units.is_empty() {
        issues.push("script must not be empty".into());
    }
    if units.len() > 65_536 {
        issues.push("script must be at most 65536 characters".into());
    }
    if units.contains(&0) {
        issues.push("script must not contain NUL bytes".into());
    }
    if String::from_utf16(units).is_err() {
        issues.push("script must be valid UTF-8 (no lone surrogates)".into());
    }
    if os == "windows" && units.iter().any(|unit| *unit >= 0x80) {
        issues.push("script must be ASCII-only on Windows (PowerShell decodes stdin with the console OEM code page); spell non-ASCII characters as escapes, e.g. [char]0x00E9".into());
    }
    issues
}

fn helper_path_unit_issues(units: &[u16], os: &str) -> Vec<String> {
    let mut issues = Vec::new();
    if units.len() > 1_024 {
        issues.push("path must be at most 1024 characters".into());
    }
    let has_forbidden = units.iter().any(|unit| {
        *unit <= 0x001f || (0x007f..=0x009f).contains(unit) || matches!(*unit, 0x2028 | 0x2029)
    }) || std::char::decode_utf16(units.iter().copied())
        .filter_map(Result::ok)
        .any(is_default_ignorable);
    if has_forbidden {
        issues.push(
            "path must not contain control, line/paragraph-separator, or invisible (default-ignorable) characters".into(),
        );
    }
    let absolute = if os == "windows" {
        windows_path_is_absolute(units)
    } else {
        units.first() == Some(&u16::from(b'/'))
    };
    if !absolute {
        issues.push("path must be absolute".into());
    }
    if os == "windows" {
        if !utf16_ascii_ends_with_ascii_case(units, b".exe")
            && !utf16_ascii_ends_with_ascii_case(units, b".ps1")
        {
            issues.push("path must end in .exe or .ps1 on Windows".into());
        }
        if utf16_ascii_ends_with_ascii_case(units, b".ps1")
            && units
                .iter()
                .any(|unit| matches!(*unit, 0x5b | 0x5d | 0x60 | 0x2a | 0x3f))
        {
            issues.push(
                "a .ps1 path must not contain \"[\", \"]\", \"`\", \"*\", or \"?\" on Windows (PowerShell resolves them as wildcard syntax)".into(),
            );
        }
        let Ok(path) = String::from_utf16(units) else {
            return issues;
        };
        if !matches!(
            validate_windows_helper_path(&path),
            HelperPathValidation::Valid
        ) {
            issues.push("path must be in normalized form: no \".\" or \"..\" segments, no doubled or trailing separators, no component ending in \".\" or a space, no \":\" outside the drive letter, no device-namespace (\\\\?\\) prefix, no drive-relative (\\dir or C:name) or UNC (\\\\server\\share) form".into());
        }
    } else if !validate_posix_helper_path(units) {
        issues.push("path must be in normalized form: no \".\" or \"..\" segments, no doubled or trailing separators, and not under a network automount root (/net/<host>, /Network/Servers, or macOS /.vol /.file /.nofollow /.resolve) or a kernel magic-link root (/proc, /dev/fd)".into());
    }
    issues
}

fn windows_path_is_absolute(units: &[u16]) -> bool {
    let slash = |unit| unit == u16::from(b'\\') || unit == u16::from(b'/');
    units.first().is_some_and(|unit| slash(*unit))
        || units.len() >= 3
            && ((u16::from(b'A')..=u16::from(b'Z')).contains(&units[0])
                || (u16::from(b'a')..=u16::from(b'z')).contains(&units[0]))
            && units[1] == u16::from(b':')
            && slash(units[2])
}

fn utf16_ascii_ends_with_ascii_case(units: &[u16], suffix: &[u8]) -> bool {
    units.len() >= suffix.len()
        && units[units.len() - suffix.len()..]
            .iter()
            .zip(suffix)
            .all(|(actual, expected)| ascii_lower_unit(*actual) == u16::from(*expected))
}

fn native_invalid_type_message_for_option(
    expected: &str,
    input: Option<JsonValueView<'_>>,
) -> String {
    match input {
        Some(input) => native_invalid_type_message(expected, input),
        None => format!("Invalid input: expected {expected}, received undefined"),
    }
}

fn invalid_option_message(options: &[&str]) -> String {
    format!(
        "Invalid option: expected one of {}",
        options
            .iter()
            .map(|value| format!("\"{value}\""))
            .collect::<Vec<_>>()
            .join("|")
    )
}

fn per_os_helper_catch_message(
    map: &Utf16JsonProjection,
    entry_key: &str,
    value: &Utf16JsonProjection,
    issues: &str,
) -> Option<String> {
    let fallback_settings = value
        .subprojection("/defaultSettings")
        .ok()
        .filter(|settings| !settings.at_pointer("").is_some_and(JsonValueView::is_null))
        .filter(passes_native_i5e_static_helper_schema);
    let output_behavior = value.subprojection("/outputBehavior").ok();
    let invalid_output_behavior = output_behavior.as_ref().is_some_and(|output| {
        !output.at_pointer("").is_some_and(JsonValueView::is_null)
            && !matches!(output.string_at(""), Some("replace" | "merge"))
    });
    let requires_helper = value
        .subprojection("/onFailure")
        .ok()
        .is_some_and(|on_failure| {
            on_failure.at_pointer("").is_some_and(|on_failure| {
                !on_failure.is_null() && on_failure.as_str() != Some("continue")
            })
        });
    let base = if fallback_settings.is_some() && !invalid_output_behavior {
        format!(
            "Invalid entry: its helper fields were ignored ({issues}), but its \"defaultSettings\" static payload was kept. No policy helper runs on {entry_key} from this entry."
        )
    } else if fallback_settings.is_some() && invalid_output_behavior {
        format!(
            "Invalid entry was ignored: {issues}. Its \"defaultSettings\" static payload was NOT kept: \"outputBehavior\" is unrecognized, so whether the payload replaces or merges over this source's settings is unknown. No policy helper runs on {entry_key} from this entry."
        )
    } else {
        format!(
            "Invalid entry was ignored: {issues}. No policy helper runs on {entry_key} from this entry."
        )
    };
    if fallback_settings.is_some() && !invalid_output_behavior {
        return Some(base);
    }
    if !requires_helper {
        return Some(base);
    }
    if has_static_fallback_for_policy_entry(map, entry_key) {
        let fallback_key = helper_static_fallback_key(map, entry_key)?;
        Some(format!(
            "{base} Its \"onFailure\" requires the helper; the \"policyHelpers.{fallback_key}\" static payload serves in its place."
        ))
    } else {
        Some(format!(
            "{base} Its \"onFailure\" requires the helper, so when delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed."
        ))
    }
}

fn helper_static_fallback_key(map: &Utf16JsonProjection, entry_key: &str) -> Option<String> {
    let platform_keys: &[&str] = if entry_key == "wsl" {
        &["wsl", "linux"]
    } else {
        match entry_key {
            "macos" | "linux" | "windows" => &[entry_key],
            _ => &[],
        }
    };
    platform_keys
        .iter()
        .find(|key| {
            helper_map_value(map, key)
                .as_ref()
                .and_then(normalize_static_fallback_only)
                .is_some()
        })
        .map(|key| format!("{key}.defaultSettings"))
        .or_else(|| {
            helper_map_value(map, "default")
                .filter(|settings| passes_native_i5e_static_helper_schema(settings))
                .map(|_| "default".to_owned())
        })
}

fn helper_entry_is_null_tombstone(value: &Utf16JsonProjection) -> bool {
    let has_null = ["path", "script", "defaultSettings"].iter().any(|field| {
        value
            .subprojection(&format!("/{field}"))
            .ok()
            .is_some_and(|value| value.at_pointer("").is_some_and(JsonValueView::is_null))
    });
    has_null
        && ["path", "script", "defaultSettings"].iter().all(|field| {
            value
                .subprojection(&format!("/{field}"))
                .ok()
                .is_none_or(|value| value.at_pointer("").is_none_or(JsonValueView::is_null))
        })
}

fn invalid_entry_requires_startup_fatal(value: &Utf16JsonProjection) -> bool {
    let explicit_non_continue = value.subprojection("/onFailure").ok().is_some_and(|value| {
        value
            .at_pointer("")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some("continue"))
    });
    explicit_non_continue
}

fn has_static_fallback_for_policy_entry(map: &Utf16JsonProjection, entry_key: &str) -> bool {
    let platform_keys: &[&str] = if entry_key == "wsl" {
        &["wsl", "linux"]
    } else {
        match entry_key {
            "macos" | "linux" | "windows" => &[entry_key],
            _ => &[],
        }
    };
    platform_keys.iter().any(|key| {
        helper_map_value(map, key)
            .as_ref()
            .and_then(normalize_static_fallback_only)
            .is_some()
    }) || helper_map_value(map, "default")
        .is_some_and(|settings| passes_native_i5e_static_helper_schema(&settings))
}

fn helper_map_value(map: &Utf16JsonProjection, key: &str) -> Option<Utf16JsonProjection> {
    map.subprojection(&format!("/{key}")).ok()
}

fn normalize_static_fallback_only(value: &Utf16JsonProjection) -> Option<Utf16JsonProjection> {
    let settings = value.subprojection("/defaultSettings").ok()?;
    if !passes_native_i5e_static_helper_schema(&settings) {
        return None;
    }
    let output_behavior = value.subprojection("/outputBehavior").ok();
    if output_behavior.as_ref().is_some_and(|output| {
        output.at_pointer("").is_some_and(|root| {
            !root.is_null() && !matches!(root.as_str(), Some("replace" | "merge"))
        })
    }) {
        return None;
    }
    let mut fields = vec![("defaultSettings".to_owned(), settings)];
    if let Some(output_behavior) =
        output_behavior.filter(|output| !output.at_pointer("").is_some_and(JsonValueView::is_null))
    {
        fields.push(("outputBehavior".to_owned(), output_behavior));
    }
    Utf16JsonProjection::object(fields).ok()
}

fn optional_helper_string(
    value: &Utf16JsonProjection,
    field: &str,
) -> Result<Option<Utf16JsonProjection>, ()> {
    let Ok(value) = value.subprojection(&format!("/{field}")) else {
        return Ok(None);
    };
    let Some(root) = value.at_pointer("") else {
        return Err(());
    };
    if root.is_null() {
        return Ok(None);
    }
    if root.code_units().is_none() {
        return Err(());
    }
    Ok(Some(value))
}

fn optional_helper_integer(
    value: &Utf16JsonProjection,
    field: &str,
    minimum: u32,
    zero_is_valid: bool,
) -> Result<Option<Utf16JsonProjection>, ()> {
    let Ok(value) = value.subprojection(&format!("/{field}")) else {
        return Ok(None);
    };
    let Some(root) = value.at_pointer("") else {
        return Err(());
    };
    if root.is_null() {
        return Ok(None);
    }
    let Some(number) = root.as_number() else {
        return Err(());
    };
    if !number.is_finite()
        || number.abs() > 9_007_199_254_740_991.0
        || number.fract() != 0.0
        || number < f64::from(minimum) && !(zero_is_valid && number == 0.0)
    {
        return Err(());
    }
    Ok(Some(number_projection(number.min(2_147_483_647.0))))
}

fn clamp_integer_projection(value: Utf16JsonProjection, maximum: u32) -> Utf16JsonProjection {
    let number = value
        .at_pointer("")
        .and_then(JsonValueView::as_number)
        .expect("optional helper integer was validated");
    number_projection(number.min(f64::from(maximum)))
}

fn number_projection(value: f64) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(&format!("{value:.0}"))
        .expect("a finite integral helper number is valid JSON")
}

enum HelperPathValidation {
    Valid,
    Invalid,
    Unresolved,
}

fn validate_helper_path(path: &Utf16JsonProjection, os: &str) -> HelperPathValidation {
    let Some(units) = path.at_pointer("").and_then(JsonValueView::code_units) else {
        return HelperPathValidation::Invalid;
    };
    if os == "windows" && String::from_utf16(units).is_err() {
        return HelperPathValidation::Unresolved;
    }
    if !helper_path_unit_issues(units, os).is_empty() {
        return HelperPathValidation::Invalid;
    }
    HelperPathValidation::Valid
}

fn validate_posix_helper_path(path: &[u16]) -> bool {
    if path
        .windows(2)
        .any(|pair| pair[0] == u16::from(b'/') && pair[1] == u16::from(b'/'))
        || path.len() > 1 && path.last() == Some(&u16::from(b'/'))
    {
        return false;
    }
    if path.split(|unit| *unit == u16::from(b'/')).any(|part| {
        part.len() == 1 && part[0] == u16::from(b'.')
            || part.len() == 2 && part[0] == u16::from(b'.') && part[1] == u16::from(b'.')
    }) {
        return false;
    }
    !is_native_network_or_magic_root_units(path)
}

fn validate_windows_helper_path(path: &str) -> HelperPathValidation {
    // Native's Eko() first converts forward slashes to backslashes, then
    // checks path.win32.normalize() and Windows-only path restrictions.
    let path = path.replace('/', "\\");
    let bytes = path.as_bytes();
    if bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
        || path.starts_with("\\\\")
        || path.contains("\\\\")
    {
        return HelperPathValidation::Invalid;
    }
    let parts = path[3..].split('\\').collect::<Vec<_>>();
    if parts.iter().any(|part| {
        part.is_empty()
            || part == &"."
            || part == &".."
            || part.ends_with('.')
            || part.ends_with(' ')
            || part.contains(':')
    }) {
        return HelperPathValidation::Invalid;
    }
    HelperPathValidation::Valid
}

fn is_native_network_or_magic_root_units(path: &[u16]) -> bool {
    const ROOTS: &[&str] = &[
        "/net",
        "/network/servers",
        "/.vol",
        "/.file",
        "/.nofollow",
        "/.resolve",
        "/proc",
        "/dev/fd",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
    ];
    ROOTS.iter().any(|root| {
        let root = root.encode_utf16().collect::<Vec<_>>();
        path.len() >= root.len()
            && path[..root.len()]
                .iter()
                .zip(&root)
                .all(|(actual, expected)| ascii_lower_unit(*actual) == *expected)
            && (path.len() == root.len() || path[root.len()] == u16::from(b'/'))
    })
}

fn ascii_lower_unit(unit: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
        unit + 32
    } else {
        unit
    }
}

fn is_default_ignorable(character: char) -> bool {
    let code = character as u32;
    matches!(
        code,
        0x00ad
            | 0x034f
            | 0x061c
            | 0x115f..=0x1160
            | 0x17b4..=0x17b5
            | 0x180b..=0x180f
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x206f
            | 0x3164
            | 0xfe00..=0xfe0f
            | 0xfeff
            | 0xffa0
            | 0xfff0..=0xfff8
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0000..=0xe0fff
    )
}

fn valid_helper_script(script: &Utf16JsonProjection, os: &str) -> bool {
    let Some(units) = script.at_pointer("").and_then(JsonValueView::code_units) else {
        return false;
    };
    !units.is_empty()
        && units.len() <= 65_536
        && !units.contains(&0)
        && String::from_utf16(units).is_ok()
        && (os != "windows" || units.iter().all(|unit| *unit <= 0x7f))
}

fn utf16_is_ascii(value: &Utf16JsonProjection, expected: &[u8]) -> bool {
    value
        .at_pointer("")
        .and_then(JsonValueView::code_units)
        .is_some_and(|units| {
            units.len() == expected.len()
                && units
                    .iter()
                    .zip(expected)
                    .all(|(unit, byte)| *unit == u16::from(*byte))
        })
}

const POLICY_HELPER_OS_KEYS: &[&str] = &["macos", "linux", "windows", "wsl"];
const STATIC_HELPER_ENTRY_FIELDS: &[&str] = &[
    "path",
    "script",
    "interpreter",
    "outputBehavior",
    "onFailure",
    "retries",
    "timeoutMs",
    "refreshIntervalMs",
    "defaultSettings",
];
const STATIC_HELPER_OUTPUT_FIELDS: &[&str] = &["managedSettings", "appendSystemPrompt"];

/// Native I5e's `pe(o(), oe()).superRefine(...)` admission. This only checks
/// that the payload is a record and rejects its exact forbidden non-null
/// properties. The later `fTr -> Due` full settings-schema validation remains
/// a separate stage.
pub(super) fn static_helper_settings_issue(value: &Utf16JsonProjection) -> Option<String> {
    let root = value.at_pointer("")?;
    if root.object_entries().is_none() {
        return Some(native_invalid_type_message("record", root));
    }

    for field in ["policyHelper", "policyHelpers"] {
        if static_payload_field_is_non_null(value, field) {
            return Some(format!(
                "must not contain \"{field}\" — the default payload is applied as managed settings and cannot configure further policy helpers"
            ));
        }
    }
    for field in STATIC_HELPER_ENTRY_FIELDS {
        if static_payload_field_is_non_null(value, field) {
            return Some(format!(
                "must not contain \"{field}\" — a static payload is a managed-settings object, not a policyHelpers entry; entry fields ({}) belong on the per-OS entries (policyHelpers.{})",
                STATIC_HELPER_ENTRY_FIELDS.join("/"),
                POLICY_HELPER_OS_KEYS.join("/")
            ));
        }
    }
    for field in STATIC_HELPER_OUTPUT_FIELDS {
        if static_payload_field_is_non_null(value, field) {
            return Some(format!(
                "must not contain \"{field}\" — a static payload is the managedSettings SUBTREE, not the helper's stdout envelope; paste the object your helper emits UNDER \"managedSettings\", not the envelope around it"
            ));
        }
    }
    for field in POLICY_HELPER_OS_KEYS.iter().copied().chain(["default"]) {
        if static_payload_field_is_non_null(value, field) {
            return Some(format!(
                "must not contain \"{field}\" — a static payload is the VALUE of a policyHelpers key (a managed-settings object), never another policyHelpers map; don't paste the map or its \"{field}\" line inside the slot"
            ));
        }
    }
    None
}

pub(super) fn passes_native_i5e_static_helper_schema(value: &Utf16JsonProjection) -> bool {
    static_helper_settings_issue(value).is_none()
}

fn static_payload_field_is_non_null(value: &Utf16JsonProjection, field: &str) -> bool {
    value
        .subprojection(&format!("/{field}"))
        .ok()
        .is_some_and(|value| value.at_pointer("").is_some_and(|value| !value.is_null()))
}

fn invalid_policy_helpers_root_message(value: &Utf16JsonProjection) -> Option<String> {
    let root = value.at_pointer("")?;
    Some(format!(
        "\"policyHelpers\" could not be parsed: expected an object mapping OS keys ({}) to helper entries, plus an optional \"default\" settings payload ({}). When delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed.",
        POLICY_HELPER_OS_KEYS.join(", "),
        native_invalid_type_message("object", root)
    ))
}

fn native_invalid_type_message(expected: &str, input: JsonValueView<'_>) -> String {
    let received = input
        .as_number()
        .filter(|number| !number.is_finite())
        .map(js_number_string)
        .unwrap_or_else(|| native_zod_input_type(input).to_owned());
    format!("Invalid input: expected {expected}, received {received}")
}

/// JSON-derived subset of Zod's imported `PPs` input-type helper. JSON.parse
/// creates ordinary objects, so its custom-constructor branch is unreachable.
fn native_zod_input_type(input: JsonValueView<'_>) -> &'static str {
    if input.is_null() {
        "null"
    } else if input.array_len().is_some() {
        "array"
    } else if input.code_units().is_some() {
        "string"
    } else if input.as_bool().is_some() {
        "boolean"
    } else if input.as_number().is_some_and(|number| number.is_nan()) {
        "NaN"
    } else if input.as_number() == Some(f64::INFINITY) {
        "Infinity"
    } else if input.as_number() == Some(f64::NEG_INFINITY) {
        "-Infinity"
    } else if input.as_number().is_some() {
        "number"
    } else {
        "object"
    }
}

pub(super) fn static_helper_startup_fatal_message(path: &str, reason: &str) -> String {
    format!(
        "\"{path}\" is not a valid static settings payload: {reason}. When delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed."
    )
}

fn push_startup_fatal_diagnostic(
    paths: &mut Vec<String>,
    diagnostics: &mut Vec<NativePolicyStartupFatalDiagnostic>,
    path: &str,
    message: Option<Utf16JsonProjection>,
) {
    if !paths.iter().any(|existing| existing == path) {
        paths.push(path.to_owned());
    }
    if let Some(existing) = diagnostics
        .iter_mut()
        .find(|existing| existing.path == path)
    {
        if existing.message.is_none() {
            existing.message = message;
        }
        return;
    }
    diagnostics.push(NativePolicyStartupFatalDiagnostic {
        path: path.to_owned(),
        message,
    });
}

fn push_unresolved_field(fields: &mut Vec<String>, field: &str) {
    if !fields.iter().any(|existing| existing == field) {
        fields.push(field.to_owned());
    }
}

fn parse_channel_plugin_id(value: &str) -> Option<(String, String)> {
    if let Some((plugin, marketplace)) = value.split_once('@') {
        if is_plugin_id_part(plugin) && is_plugin_id_part(marketplace) {
            return Some((plugin.to_owned(), marketplace.to_owned()));
        }
    }
    let package = value.strip_suffix("@npm")?;
    if package.contains("..") || package.contains("//") {
        return None;
    }
    let valid_package = if let Some(scoped) = package.strip_prefix('@') {
        let (scope, name) = scoped.split_once('/')?;
        is_npm_package_part(scope) && is_npm_package_part(name)
    } else {
        is_npm_package_part(package)
    };
    valid_package.then(|| {
        let at = value.find('@').expect("npm suffix has an at sign");
        (value[..at].to_owned(), value[at + 1..].to_owned())
    })
}

fn is_plugin_id_part(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn is_npm_package_part(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '-' | '_' | '.')
        })
}

fn normalize_gateway_internal_networks(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    const FIELD: &str = "gatewayInternalNetworks";
    let pointer = format!("/{FIELD}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let valid = value
        .at_pointer("")
        .and_then(JsonValueView::array_len)
        .is_some_and(|length| {
            (0..length).all(|index| {
                value
                    .at_pointer("")
                    .and_then(|root| root.get_index(index))
                    .and_then(JsonValueView::as_str)
                    .is_some()
            })
        });
    if valid {
        return;
    }
    let fallback = Utf16JsonProjection::array(vec![string_projection("(unreadable)")])
        .expect("gateway network fallback is valid JSON");
    let _ = projection.set_field(FIELD, fallback);
    diagnostics.push(diagnostic(
        pointer,
        NativePolicyDiagnosticKind::FallbackValue,
        false,
        false,
        false,
    ));
}

fn normalize_force_login_org_uuid(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    const FIELD: &str = "forceLoginOrgUUID";
    let pointer = format!("/{FIELD}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    let valid = root.as_str().is_some()
        || root.array_len().is_some_and(|length| {
            (0..length).all(|index| {
                root.get_index(index)
                    .and_then(JsonValueView::as_str)
                    .is_some()
            })
        });
    if valid {
        return;
    }
    let _ = projection.set_field(FIELD, empty_array());
    diagnostics.push(diagnostic(
        pointer,
        NativePolicyDiagnosticKind::FallbackEmpty,
        false,
        false,
        false,
    ));
}

fn normalize_permission_locks(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
    synthesized_paths: &mut Vec<String>,
) {
    // `Bo("permissions", ...)` synthesizes these fields in the schema's
    // order: base permissions fields, then autoMode's injected shape. Other
    // permission fields remain the responsibility of their own parser slice.
    let skeleton = Utf16JsonProjection::object(vec![
        (
            "disableBypassPermissionsMode".into(),
            string_projection("disable"),
        ),
        (
            "blockReadsOutsideWorkingDirectories".into(),
            bool_projection(true),
        ),
        ("disableAutoMode".into(), string_projection("disable")),
    ])
    .expect("permissions restrictive skeleton is valid JSON");
    if !normalize_restricted_object_root(
        projection,
        "permissions",
        skeleton,
        diagnostics,
        substituted_paths,
        synthesized_paths,
    ) {
        return;
    }
    normalize_disable_only_path(
        projection,
        &["permissions", "disableBypassPermissionsMode"],
        diagnostics,
        substituted_paths,
    );
    normalize_disable_only_path(
        projection,
        &["permissions", "disableAutoMode"],
        diagnostics,
        substituted_paths,
    );
}

/// Apply `Bo`'s null-removal and invalid-root restrictive-skeleton branches
/// for an object with Native lock fields. Returns true only when an input
/// object remains to have its fields normalized individually.
fn normalize_restricted_object_root(
    projection: &mut Utf16JsonProjection,
    field: &str,
    skeleton: Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    substituted_paths: &mut Vec<String>,
    synthesized_paths: &mut Vec<String>,
) -> bool {
    let pointer = format!("/{field}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return false;
    };
    let Some(root) = value.at_pointer("") else {
        return false;
    };
    if root.is_null() {
        let _ = projection.remove_field(field);
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Removed,
            false,
            false,
            false,
        ));
        return false;
    }
    if root.object_entries().is_none() {
        let _ = projection.set_field(field, skeleton);
        substituted_paths.push(pointer.clone());
        synthesized_paths.push(pointer.clone());
        diagnostics.push(diagnostic(
            pointer,
            NativePolicyDiagnosticKind::Synthesized,
            true,
            true,
            false,
        ));
        return false;
    }
    true
}

fn normalize_available_models(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    normalize_string_array(projection, "availableModels", false, diagnostics);
}

/// Apply Native's `os` repair for `enabledPlugins`. The map retains every row
/// whose value is either a boolean or an array consisting entirely of strings;
/// arrays are validated as a whole. The plugin-id schema is used only to pick
/// the diagnostic path for an invalid value and never filters a valid row.
fn normalize_enabled_plugins(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    let Some(value) = projection.subprojection("/enabledPlugins").ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    if root.object_entries().is_none() {
        let _ = projection.remove_field("enabledPlugins");
        diagnostics.push(diagnostic(
            "/enabledPlugins".into(),
            NativePolicyDiagnosticKind::Ignored,
            false,
            false,
            false,
        ));
        return;
    }

    let mut retained = Vec::new();
    let mut invalid_entries = 0;
    for (key_units, entry_value) in object_entries_in_javascript_order(&value) {
        if is_enabled_plugin_value(&entry_value) {
            retained.push((key_units, entry_value));
            continue;
        }
        invalid_entries += 1;
        let path = String::from_utf16(&key_units)
            .ok()
            .filter(|key| native_plugin_id_is_valid(key))
            .map(|key| format!("/enabledPlugins/{}", escape_pointer_token(&key)))
            .unwrap_or_else(|| "/enabledPlugins/<invalid id>".into());
        diagnostics.push(diagnostic(
            path,
            NativePolicyDiagnosticKind::InvalidEntryIgnored,
            false,
            false,
            false,
        ));
    }

    if invalid_entries > 0 && retained.is_empty() {
        let _ = projection.remove_field("enabledPlugins");
        diagnostics.push(diagnostic(
            "/enabledPlugins".into(),
            NativePolicyDiagnosticKind::Ignored,
            false,
            false,
            false,
        ));
        return;
    }

    if let Ok(normalized) = Utf16JsonProjection::object_units(retained) {
        let _ = projection.set_field("enabledPlugins", normalized);
    }
}

/// Native registers `prependPlugins` and `appendPlugins` as optional string
/// arrays with `.catch(undefined)`: a valid array is retained byte-for-byte,
/// while any invalid shape or member makes the entire field absent.
fn normalize_caught_string_array(
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
        .and_then(JsonValueView::array_len)
        .is_some_and(|length| {
            (0..length).all(|index| {
                value
                    .at_pointer("")
                    .and_then(|root| root.get_index(index))
                    .and_then(JsonValueView::code_units)
                    .is_some()
            })
        });
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

/// Native `deniedModels` keeps string members from an array and drops each
/// non-string member. A non-array value is caught as unset.
fn normalize_denied_models(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    const FIELD: &str = "deniedModels";
    let Some(value) = projection.subprojection("/deniedModels").ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    let Some(length) = root.array_len() else {
        let _ = projection.remove_field(FIELD);
        diagnostics.push(diagnostic(
            "/deniedModels".into(),
            NativePolicyDiagnosticKind::Ignored,
            false,
            false,
            false,
        ));
        return;
    };
    let mut retained = Vec::new();
    for index in 0..length {
        let is_string = root
            .get_index(index)
            .and_then(JsonValueView::code_units)
            .is_some();
        if is_string {
            if let Ok(item) = value.subprojection(&format!("/{index}")) {
                retained.push(item);
            }
        } else {
            diagnostics.push(diagnostic(
                "/deniedModels".into(),
                NativePolicyDiagnosticKind::InvalidEntryIgnored,
                false,
                false,
                false,
            ));
        }
    }
    if let Ok(array) = Utf16JsonProjection::array(retained) {
        let _ = projection.set_field(FIELD, array);
    }
}

/// Normalize Native's `allowedMcpServers` / `deniedMcpServers` arrays. Each
/// accepted entry is a strict one-of object; unknown object keys are stripped
/// by the source schema, and invalid entries are removed individually.
fn normalize_mcp_server_list(
    projection: &mut Utf16JsonProjection,
    field: &str,
    allowlist: bool,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    let pointer = format!("/{field}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let Some(root) = value.at_pointer("") else {
        return;
    };
    let Some(length) = root.array_len() else {
        if allowlist {
            let _ = projection.set_field(field, empty_array());
            diagnostics.push(diagnostic(
                pointer,
                NativePolicyDiagnosticKind::FallbackEmpty,
                false,
                false,
                false,
            ));
        } else {
            let _ = projection.remove_field(field);
            diagnostics.push(diagnostic(
                pointer,
                NativePolicyDiagnosticKind::Ignored,
                false,
                false,
                false,
            ));
        }
        return;
    };

    let mut retained = Vec::new();
    for index in 0..length {
        let Ok(item) = value.subprojection(&format!("/{index}")) else {
            continue;
        };
        let Some(normalized) = normalize_mcp_server_entry(&item, allowlist) else {
            diagnostics.push(diagnostic(
                format!("{pointer}/{index}"),
                NativePolicyDiagnosticKind::InvalidEntryIgnored,
                false,
                false,
                false,
            ));
            continue;
        };
        retained.push(normalized);
    }
    if allowlist && length > 0 && retained.is_empty() {
        diagnostics.push(diagnostic(
            pointer.clone(),
            NativePolicyDiagnosticKind::FallbackEmpty,
            false,
            false,
            false,
        ));
    }
    if let Ok(array) = Utf16JsonProjection::array(retained) {
        let _ = projection.set_field(field, array);
    }
}

pub(super) fn normalize_mcp_server_entry(
    source: &Utf16JsonProjection,
    allowlist: bool,
) -> Option<Utf16JsonProjection> {
    let root = source.at_pointer("")?;
    root.object_entries()?;
    let mut fields = Vec::new();
    let mut selected = 0;

    if let Ok(server_name) = source.subprojection("/serverName") {
        let units = server_name.at_pointer("")?.code_units()?;
        if allowlist {
            if !units
                .iter()
                .all(|unit| matches!(unit, 45 | 48..=57 | 65..=90 | 95 | 97..=122))
            {
                return None;
            }
        } else if units.is_empty()
            || js_trimmed_units(units) != units
            || units.iter().all(|unit| is_js_trim_space(*unit))
        {
            return None;
        }
        fields.push(("serverName".into(), server_name));
        selected += 1;
    }

    if let Ok(server_command) = source.subprojection("/serverCommand") {
        let Some(command_root) = server_command.at_pointer("") else {
            return None;
        };
        let Some(length) = command_root.array_len() else {
            return None;
        };
        if length == 0
            || !(0..length).all(|index| {
                command_root
                    .get_index(index)
                    .and_then(JsonValueView::code_units)
                    .is_some()
            })
        {
            return None;
        }
        fields.push(("serverCommand".into(), server_command));
        selected += 1;
    }

    if let Ok(server_url) = source.subprojection("/serverUrl") {
        server_url.at_pointer("")?.code_units()?;
        fields.push(("serverUrl".into(), server_url));
        selected += 1;
    }

    if selected != 1 {
        return None;
    }
    Utf16JsonProjection::object(fields).ok()
}

fn js_trimmed_units(value: &[u16]) -> &[u16] {
    let start = value
        .iter()
        .position(|unit| !is_js_trim_space(*unit))
        .unwrap_or(value.len());
    let end = value
        .iter()
        .rposition(|unit| !is_js_trim_space(*unit))
        .map_or(start, |index| index + 1);
    &value[start..end]
}

fn is_js_trim_space(unit: u16) -> bool {
    matches!(
        unit,
        0x0009..=0x000d
            | 0x0020
            | 0x00a0
            | 0x1680
            | 0x2000..=0x200a
            | 0x2028..=0x2029
            | 0x202f
            | 0x205f
            | 0x3000
            | 0xfeff
    )
}

fn is_enabled_plugin_value(value: &Utf16JsonProjection) -> bool {
    let Some(root) = value.at_pointer("") else {
        return false;
    };
    if root.as_bool().is_some() {
        return true;
    }
    let Some(length) = root.array_len() else {
        return false;
    };
    (0..length).all(|index| {
        root.get_index(index)
            .and_then(JsonValueView::code_units)
            .is_some()
    })
}

/// Apply Native `Nue` alias moves before policy-field schema normalization.
/// A canonical non-null value wins; an explicit null alias is simply removed
/// in this managed-policy caller because `At` passes no `loneNullAlias` mode.
fn normalize_marketplace_aliases(
    projection: &mut Utf16JsonProjection,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
) {
    for (alias, canonical) in [
        ("additionalMarketplaces", "extraKnownMarketplaces"),
        ("allowedMarketplaces", "strictKnownMarketplaces"),
    ] {
        let Some(alias_projection) = projection.subprojection(&format!("/{alias}")).ok() else {
            continue;
        };
        let is_null = alias_projection
            .at_pointer("")
            .is_some_and(JsonValueView::is_null);
        if is_null {
            let _ = projection.remove_field(alias);
            continue;
        }
        let canonical_nonnull = projection
            .subprojection(&format!("/{canonical}"))
            .ok()
            .is_some_and(|value| value.at_pointer("").is_some_and(|value| !value.is_null()));
        if canonical_nonnull {
            let _ = projection.remove_field(alias);
            diagnostics.push(diagnostic(
                format!("/{alias}"),
                NativePolicyDiagnosticKind::Ignored,
                false,
                false,
                false,
            ));
        } else {
            let _ = projection.set_field(canonical, alias_projection);
            let _ = projection.remove_field(alias);
        }
    }
}

fn normalize_policy_marketplace_field(
    projection: &mut Utf16JsonProjection,
    field: &str,
    diagnostics: &mut Vec<NativePolicyParseDiagnostic>,
    unresolved_fields: &mut Vec<String>,
) {
    let pointer = format!("/{field}");
    let Some(value) = projection.subprojection(&pointer).ok() else {
        return;
    };
    let result = super::native_policy_marketplaces::normalize_policy_marketplace_field(
        field,
        &value,
        &[],
        None,
    );
    if let Some(normalized) = result.normalized {
        let _ = projection.set_field(field, normalized);
    } else {
        let _ = projection.remove_field(field);
    }
    if result.unresolved && !unresolved_fields.iter().any(|item| item == field) {
        unresolved_fields.push(field.to_owned());
    }
    diagnostics.extend(result.diagnostics);
}

/// Native's `Object.entries` enumerates canonical array-index keys first in
/// ascending numeric order, then other keys in source insertion order. The
/// projection parser preserves JSON source order, so sort only index keys
/// before applying the `enabledPlugins` record transform.
fn object_entries_in_javascript_order(
    value: &Utf16JsonProjection,
) -> Vec<(Vec<u16>, Utf16JsonProjection)> {
    let Some(entries) = value.object_entries() else {
        return Vec::new();
    };
    let mut indexed = Vec::new();
    let mut rest = Vec::new();
    for entry in entries {
        let key_units = entry.code_units.to_vec();
        let entry_value = value
            .subprojection_property_units(entry.code_units)
            .expect("object entry exists in its source projection");
        let array_index = String::from_utf16(&key_units).ok().and_then(|key| {
            key.parse::<u32>()
                .ok()
                .filter(|index| *index != u32::MAX && index.to_string() == key)
        });
        if let Some(index) = array_index {
            indexed.push((index, key_units, entry_value));
        } else {
            rest.push((key_units, entry_value));
        }
    }
    indexed.sort_unstable_by_key(|(index, _, _)| *index);
    indexed
        .into_iter()
        .map(|(_, key, value)| (key, value))
        .chain(rest)
        .collect()
}

/// The `fO()` plugin-id refinement only chooses the Native diagnostic path;
/// valid map entries are retained even when this returns false.
fn native_plugin_id_is_valid(value: &str) -> bool {
    let match_component = |component: &str| {
        let mut bytes = component.bytes();
        bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte))
    };

    let plugin_marketplace = value
        .split_once('@')
        .filter(|(plugin, marketplace)| {
            !marketplace.contains('@') && match_component(plugin) && match_component(marketplace)
        })
        .is_some();
    if plugin_marketplace {
        return true;
    }

    let Some(package) = value.strip_suffix("@npm") else {
        return false;
    };
    if package.contains("..") || package.contains("//") {
        return false;
    }
    let package_component = |part: &str| {
        let bytes = part.as_bytes();
        !bytes.is_empty()
            && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
            && bytes[1..].iter().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-._".contains(byte)
            })
    };
    if let Some(scoped) = package.strip_prefix('@') {
        let Some((scope, name)) = scoped.split_once('/') else {
            return false;
        };
        !name.contains('/') && package_component(scope) && package_component(name)
    } else {
        !package.contains('/') && package_component(package)
    }
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
    normalize_disable_only_path(projection, &[field], diagnostics, substituted_paths);
}

fn normalize_disable_only_path(
    projection: &mut Utf16JsonProjection,
    path: &[&str],
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
    if root.is_null() || root.as_bool() == Some(false) || root.as_str() == Some("false") {
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
    if root.as_str() == Some("disable") {
        return;
    }
    set_path(projection, path, string_projection("disable"));
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
    if let Ok(sandbox) = projection.subprojection("/sandbox") {
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
    // `disableAllHooks` is an ordinary optional boolean in `SettingsJson`,
    // not a tolerant `Qo` field. A well-typed value needs no transformation;
    // a present non-boolean makes Native's whole policy `safeParse` fail.
    if projection
        .subprojection("/disableAllHooks")
        .ok()
        .is_some_and(|value| {
            value
                .at_pointer("")
                .and_then(JsonValueView::as_bool)
                .is_none()
        })
    {
        unresolved.push("disableAllHooks".to_owned());
    }
    if let Ok(sandbox) = projection.subprojection("/sandbox") {
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
            if let Ok(child) = sandbox.subprojection(&format!("/{object}")) {
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
        parse_policy_document(raw)
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
    fn source_oracle_enabled_plugins_keeps_boolean_and_utf16_string_array_values() {
        let result = parse(
            r#"{"enabledPlugins":{"formatter@marketplace":true,"opaque/id":false,"release@marketplace":["not-a-semver-parser-input","\ud800"]}}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        let plugins = normalized.subprojection("/enabledPlugins").unwrap();
        assert_eq!(plugins.object_entries().unwrap().len(), 3);
        assert_eq!(
            plugins
                .at_pointer("/formatter@marketplace")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        // fO() is diagnostic-only: an invalid plugin ID survives if its value
        // passes va().
        assert_eq!(
            plugins
                .at_pointer("/opaque~1id")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(plugins.array_len("/release@marketplace"), Some(2));
        assert_eq!(
            plugins.string_units("/release@marketplace/1"),
            Some(vec![0xd800])
        );
        assert!(result
            .unresolved_fields
            .iter()
            .all(|field| field != "enabledPlugins"));
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn source_oracle_enabled_plugins_drops_whole_invalid_rows_and_uses_js_key_order() {
        let result = parse(
            r#"{"enabledPlugins":{"z@marketplace":true,"10":false,"2":true,"01":false,"broken@marketplace":["^1",17],"bad/value":null}}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        let plugins = normalized.subprojection("/enabledPlugins").unwrap();
        assert_eq!(plugins.object_entries().unwrap().len(), 4);
        assert_eq!(
            plugins.to_json_string().unwrap(),
            r#"{"2":true,"10":false,"z@marketplace":true,"01":false}"#
        );
        assert!(result.diagnostics.iter().any(|item| {
            item.path == "/enabledPlugins/broken@marketplace"
                && item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored
        }));
        assert!(result.diagnostics.iter().any(|item| {
            item.path == "/enabledPlugins/<invalid id>"
                && item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored
        }));
    }

    #[test]
    fn source_oracle_enabled_plugins_omits_invalid_or_all_invalid_fields_but_keeps_empty_map() {
        let all_invalid = parse(
            r#"{"enabledPlugins":{"bad@marketplace":17,"mixed@marketplace":["^1",null]},"sibling":"retained"}"#,
        );
        let normalized = all_invalid.normalized_projection.as_ref().unwrap();
        assert!(!normalized.contains_property("enabledPlugins"));
        assert_eq!(normalized.string_at("/sibling"), Some("retained"));
        assert_eq!(all_invalid.document_has_policy_content, Some(true));
        assert!(all_invalid.diagnostics.iter().any(|item| {
            item.path == "/enabledPlugins" && item.kind == NativePolicyDiagnosticKind::Ignored
        }));

        let invalid_root = parse(r#"{"enabledPlugins":null,"sibling":true}"#);
        assert!(!invalid_root
            .normalized_projection
            .as_ref()
            .unwrap()
            .contains_property("enabledPlugins"));
        assert_eq!(
            invalid_root
                .normalized_projection
                .as_ref()
                .unwrap()
                .at_pointer("/sibling")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );

        let empty = parse(r#"{"enabledPlugins":{}}"#);
        assert_eq!(
            empty
                .normalized_projection
                .as_ref()
                .unwrap()
                .subprojection("/enabledPlugins")
                .unwrap()
                .object_entries()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn source_oracle_enabled_plugins_invalid_key_diagnostic_uses_plugin_id_refinement() {
        let result = parse(
            r#"{"enabledPlugins":{"plugin@marketplace":17,"@scope/package@npm":null,"bad key":17}}"#,
        );
        let paths = result
            .diagnostics
            .iter()
            .filter(|item| item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored)
            .map(|item| item.path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            paths,
            vec![
                "/enabledPlugins/plugin@marketplace",
                "/enabledPlugins/@scope~1package@npm",
                "/enabledPlugins/<invalid id>"
            ]
        );
        assert!(native_plugin_id_is_valid("plugin@marketplace"));
        assert!(native_plugin_id_is_valid("@scope/package@npm"));
        assert!(!native_plugin_id_is_valid("bad key"));
        assert!(!native_plugin_id_is_valid("plugin@marketplace\n"));
        assert!(!native_plugin_id_is_valid("@scope/package\n@npm"));
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
    fn source_oracle_auto_mode_classifier_coerces_removes_and_fails_closed() {
        let valid_false = parse(
            r#"{"autoMode":{"classifyAllShell":false,"useAutoModeDuringPlan":false},"model":"kept"}"#,
        );
        let valid_false_projection = valid_false.normalized_projection.as_ref().unwrap();
        assert_eq!(
            valid_false_projection
                .at_pointer("/autoMode/classifyAllShell")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(!valid_false
            .unresolved_fields
            .iter()
            .any(|field| field == "autoMode.classifyAllShell"));
        assert!(valid_false.substituted_paths.is_empty());

        let coerced = parse(r#"{"autoMode":{"classifyAllShell":"false"}}"#);
        assert_eq!(
            coerced
                .normalized_projection
                .as_ref()
                .unwrap()
                .at_pointer("/autoMode/classifyAllShell")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(coerced.diagnostics.iter().any(|item| {
            item.path == "/autoMode/classifyAllShell"
                && item.kind == NativePolicyDiagnosticKind::Coerced
                && item.status_only
                && !item.substituted
        }));

        let null = parse(r#"{"autoMode":{"classifyAllShell":null}}"#);
        assert!(!null
            .normalized_projection
            .as_ref()
            .unwrap()
            .contains_property("autoMode"));
        assert!(null.diagnostics.iter().any(|item| {
            item.path == "/autoMode/classifyAllShell"
                && item.kind == NativePolicyDiagnosticKind::Removed
                && item.status_only
        }));

        let invalid = parse(
            r#"{"model":"kept","autoMode":{"classifyAllShell":"yes","useAutoModeDuringPlan":false}}"#,
        );
        let normalized = invalid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/autoMode/classifyAllShell")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            normalized
                .at_pointer("/autoMode/useAutoModeDuringPlan")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert_eq!(
            invalid.substituted_paths,
            vec!["/autoMode/classifyAllShell"]
        );
        assert!(!invalid.only_substitutes);

        let invalid_root = parse(r#"{"model":"kept","autoMode":"bad"}"#);
        let normalized = invalid_root.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/autoMode/classifyAllShell")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(invalid_root.substituted_paths, vec!["/autoMode"]);
        assert_eq!(invalid_root.synthesized_paths, vec!["/autoMode"]);
        assert!(!invalid_root.only_substitutes);

        let absent = parse(r#"{"model":"kept"}"#);
        assert!(!absent
            .normalized_projection
            .as_ref()
            .unwrap()
            .contains_property("autoMode"));
        assert!(absent.substituted_paths.is_empty());
        assert!(absent.synthesized_paths.is_empty());
    }

    #[test]
    fn source_oracle_permission_disable_fields_remove_false_and_substitute_invalid() {
        let normalized = parse(
            r#"{"permissions":{"disableBypassPermissionsMode":false,"allow":["Read(*)"],"disableAutoMode":"false"},"sibling":true}"#,
        );
        let projection = normalized.normalized_projection.as_ref().unwrap();
        assert_eq!(projection.array_len("/permissions/allow"), Some(1));
        assert!(projection
            .at_pointer("/permissions/disableBypassPermissionsMode")
            .is_none());
        assert!(projection
            .at_pointer("/permissions/disableAutoMode")
            .is_none());
        assert_eq!(
            projection
                .at_pointer("/sibling")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            normalized
                .diagnostics
                .iter()
                .filter(|item| {
                    item.kind == NativePolicyDiagnosticKind::Removed && item.status_only
                })
                .count(),
            2
        );

        let null = parse(
            r#"{"permissions":{"disableBypassPermissionsMode":null,"disableAutoMode":"disable","allow":[]}}"#,
        );
        let projection = null.normalized_projection.as_ref().unwrap();
        assert!(projection
            .at_pointer("/permissions/disableBypassPermissionsMode")
            .is_none());
        assert_eq!(
            projection.string_at("/permissions/disableAutoMode"),
            Some("disable")
        );
        assert_eq!(projection.array_len("/permissions/allow"), Some(0));

        let invalid = parse(
            r#"{"sibling":"retained","permissions":{"disableBypassPermissionsMode":true,"disableAutoMode":"enabled"}}"#,
        );
        let projection = invalid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            projection.string_at("/permissions/disableBypassPermissionsMode"),
            Some("disable")
        );
        assert_eq!(
            projection.string_at("/permissions/disableAutoMode"),
            Some("disable")
        );
        assert_eq!(projection.string_at("/sibling"), Some("retained"));
        assert_eq!(
            invalid.substituted_paths,
            vec![
                "/permissions/disableBypassPermissionsMode",
                "/permissions/disableAutoMode"
            ]
        );
        assert!(!invalid.only_substitutes);

        let invalid_root = parse(r#"{"permissions":"bad"}"#);
        let projection = invalid_root.normalized_projection.as_ref().unwrap();
        assert_eq!(
            projection.string_at("/permissions/disableBypassPermissionsMode"),
            Some("disable")
        );
        assert_eq!(
            projection
                .at_pointer("/permissions/blockReadsOutsideWorkingDirectories")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            projection.string_at("/permissions/disableAutoMode"),
            Some("disable")
        );
        assert_eq!(invalid_root.substituted_paths, vec!["/permissions"]);
        assert_eq!(invalid_root.synthesized_paths, vec!["/permissions"]);
        assert!(invalid_root.only_substitutes);

        let null_root = parse(r#"{"permissions":null}"#);
        assert!(!null_root
            .normalized_projection
            .as_ref()
            .unwrap()
            .contains_property("permissions"));
        assert!(null_root.substituted_paths.is_empty());
        assert!(null_root.synthesized_paths.is_empty());
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
    fn source_oracle_simple_caught_policy_fields_preserve_valid_and_drop_invalid() {
        let result = parse(
            r#"{"strictPluginOnlyCustomization":["skills","typo",17,"mcp"],"modelProposedGoals":"alwaysAsk","feedbackDrafts":"quiet","askUserQuestionTimeout":"5m","dialogExpiry":"never","crossSessionInbound":"hold","model":"kept"}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.array_len("/strictPluginOnlyCustomization"),
            Some(2)
        );
        assert_eq!(
            normalized.string_at("/strictPluginOnlyCustomization/0"),
            Some("skills")
        );
        assert_eq!(
            normalized.string_at("/strictPluginOnlyCustomization/1"),
            Some("mcp")
        );
        for (path, expected) in [
            ("/modelProposedGoals", "alwaysAsk"),
            ("/feedbackDrafts", "quiet"),
            ("/askUserQuestionTimeout", "5m"),
            ("/dialogExpiry", "never"),
            ("/crossSessionInbound", "hold"),
        ] {
            assert_eq!(normalized.string_at(path), Some(expected), "{path}");
        }
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert!(result.unresolved_fields.is_empty());

        let invalid = parse(
            r#"{"strictPluginOnlyCustomization":"bad","modelProposedGoals":null,"feedbackDrafts":"loud","askUserQuestionTimeout":15,"dialogExpiry":{},"crossSessionInbound":"default"}"#,
        );
        let normalized = invalid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/strictPluginOnlyCustomization")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        for path in [
            "/modelProposedGoals",
            "/feedbackDrafts",
            "/askUserQuestionTimeout",
            "/dialogExpiry",
            "/crossSessionInbound",
        ] {
            assert!(normalized.at_pointer(path).is_none(), "{path}");
        }
        assert_eq!(
            invalid.substituted_paths,
            vec!["/strictPluginOnlyCustomization"]
        );
        assert!(invalid.unresolved_fields.is_empty());
    }

    #[test]
    fn source_oracle_plugin_order_arrays_drop_invalid_fields_and_keep_utf16() {
        let result = parse(
            r#"{"prependPlugins":["first","\ud800"],"appendPlugins":["last"],"model":"kept"}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(normalized.string_at("/prependPlugins/0"), Some("first"));
        assert_eq!(
            normalized
                .at_pointer("/prependPlugins/1")
                .and_then(JsonValueView::code_units),
            Some(&[0xd800][..])
        );
        assert_eq!(normalized.string_at("/appendPlugins/0"), Some("last"));
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert!(result.unresolved_fields.is_empty());

        let invalid =
            parse(r#"{"prependPlugins":["first",17],"appendPlugins":null,"model":"kept"}"#);
        let normalized = invalid.normalized_projection.as_ref().unwrap();
        assert!(normalized.at_pointer("/prependPlugins").is_none());
        assert!(normalized.at_pointer("/appendPlugins").is_none());
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert_eq!(
            invalid
                .diagnostics
                .iter()
                .filter(|item| item.kind == NativePolicyDiagnosticKind::Ignored)
                .count(),
            2
        );
    }

    #[test]
    fn source_oracle_denied_models_filters_members_and_catches_wrong_root() {
        let result = parse(r#"{"deniedModels":["claude-opus","\ud800",17,null],"model":"kept"}"#);
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(normalized.array_len("/deniedModels"), Some(2));
        assert_eq!(normalized.string_at("/deniedModels/0"), Some("claude-opus"));
        assert_eq!(
            normalized
                .at_pointer("/deniedModels/1")
                .and_then(JsonValueView::code_units),
            Some(&[0xd800][..])
        );
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert_eq!(
            result
                .diagnostics
                .iter()
                .filter(|item| {
                    item.path == "/deniedModels"
                        && item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored
                })
                .count(),
            2
        );

        let wrong_root = parse(r#"{"deniedModels":null,"model":"kept"}"#);
        let normalized = wrong_root.normalized_projection.as_ref().unwrap();
        assert!(normalized.at_pointer("/deniedModels").is_none());
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert!(wrong_root.unresolved_fields.is_empty());
    }

    #[test]
    fn source_oracle_available_models_match_invalid_is_unset_not_fail_closed() {
        let valid = parse(r#"{"availableModelsMatch":"exact"}"#);
        assert_eq!(
            valid
                .normalized_projection
                .as_ref()
                .unwrap()
                .string_at("/availableModelsMatch"),
            Some("exact")
        );

        let invalid = parse(r#"{"availableModelsMatch":"typo","model":"kept"}"#);
        let normalized = invalid.normalized_projection.as_ref().unwrap();
        assert!(normalized.at_pointer("/availableModelsMatch").is_none());
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert!(invalid.unresolved_fields.is_empty());
        assert!(invalid.diagnostics.iter().any(|item| {
            item.path == "/availableModelsMatch" && item.kind == NativePolicyDiagnosticKind::Ignored
        }));
    }

    #[test]
    fn source_oracle_permission_read_lock_and_ordinary_hook_bool_are_distinct() {
        let valid = parse(
            r#"{"permissions":{"blockReadsOutsideWorkingDirectories":false,"allow":["Read(src/**)"]},"disableAllHooks":false}"#,
        );
        let normalized = valid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/permissions/blockReadsOutsideWorkingDirectories")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert_eq!(normalized.array_len("/permissions/allow"), Some(1));
        assert_eq!(
            normalized
                .at_pointer("/disableAllHooks")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
        assert!(valid.unresolved_fields.is_empty());

        let repaired_lock = parse(
            r#"{"permissions":{"blockReadsOutsideWorkingDirectories":"bad","allow":["Read(src/**)"]},"model":"kept"}"#,
        );
        let normalized = repaired_lock.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/permissions/blockReadsOutsideWorkingDirectories")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(normalized.array_len("/permissions/allow"), Some(1));
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert_eq!(
            repaired_lock.substituted_paths,
            vec!["/permissions/blockReadsOutsideWorkingDirectories"]
        );

        let strict_invalid = parse(r#"{"disableAllHooks":"true","model":"kept"}"#);
        assert!(strict_invalid
            .unresolved_fields
            .contains(&"disableAllHooks".to_owned()));
        assert_eq!(
            strict_invalid
                .normalized_projection
                .as_ref()
                .unwrap()
                .string_at("/disableAllHooks"),
            Some("true")
        );
    }

    #[test]
    fn source_oracle_gateway_and_org_pin_fallbacks_preserve_valid_source_values() {
        let valid = parse(
            r#"{"gatewayInternalNetworks":["203.0.113.0/24"],"forceLoginOrgUUID":"org-name"}"#,
        );
        let normalized = valid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/gatewayInternalNetworks/0"),
            Some("203.0.113.0/24")
        );
        assert_eq!(normalized.string_at("/forceLoginOrgUUID"), Some("org-name"));
        assert!(valid.substituted_paths.is_empty());

        let invalid = parse(
            r#"{"gatewayInternalNetworks":["203.0.113.0/24",17],"forceLoginOrgUUID":{"org":"x"},"model":"kept"}"#,
        );
        let normalized = invalid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/gatewayInternalNetworks/0"),
            Some("(unreadable)")
        );
        assert_eq!(normalized.array_len("/forceLoginOrgUUID"), Some(0));
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert!(invalid.substituted_paths.is_empty());
        assert!(invalid.unresolved_fields.is_empty());
    }

    #[test]
    fn source_oracle_allowed_providers_filters_entries_and_fails_closed_on_bad_root() {
        let result =
            parse(r#"{"allowedProviders":["bedrock","anthropicFast","unknown",17,"vertex"]}"#);
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(normalized.array_len("/allowedProviders"), Some(3));
        assert_eq!(normalized.string_at("/allowedProviders/0"), Some("bedrock"));
        assert_eq!(
            normalized.string_at("/allowedProviders/1"),
            Some("anthropicFast")
        );
        assert_eq!(normalized.string_at("/allowedProviders/2"), Some("vertex"));
        assert!(result.unresolved_fields.is_empty());
        assert!(result.diagnostics.iter().any(|item| {
            item.path == "/allowedProviders/2"
                && item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored
                && item.status_only
        }));

        let invalid_root = parse(r#"{"allowedProviders":{"bedrock":true}}"#);
        assert_eq!(
            invalid_root
                .normalized_projection
                .as_ref()
                .unwrap()
                .array_len("/allowedProviders"),
            Some(0)
        );
        assert!(invalid_root.unresolved_fields.is_empty());
    }

    #[test]
    fn source_oracle_channel_plugin_allowlist_normalizes_legacy_and_keeps_raw_strings() {
        let result = parse(
            r#"{"allowedChannelPlugins":["local@market","bad id",{"marketplace":"x\ud800","plugin":"alerts","ignored":true},null]}"#,
        );
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert_eq!(normalized.array_len("/allowedChannelPlugins"), Some(2));
        assert_eq!(
            normalized.string_at("/allowedChannelPlugins/0/marketplace"),
            Some("market")
        );
        assert_eq!(
            normalized.string_at("/allowedChannelPlugins/0/plugin"),
            Some("local")
        );
        assert_eq!(
            normalized
                .at_pointer("/allowedChannelPlugins/1/marketplace")
                .and_then(JsonValueView::code_units),
            Some(&['x' as u16, 0xd800][..])
        );
        assert!(normalized
            .at_pointer("/allowedChannelPlugins/1/ignored")
            .is_none());
        assert_eq!(
            normalized.string_at("/allowedChannelPlugins/1/plugin"),
            Some("alerts")
        );
        assert!(result.unresolved_fields.is_empty());

        let all_invalid = parse(r#"{"allowedChannelPlugins":["bad id",17]}"#);
        assert_eq!(
            all_invalid
                .normalized_projection
                .as_ref()
                .unwrap()
                .array_len("/allowedChannelPlugins"),
            Some(0)
        );
        assert!(all_invalid.unresolved_fields.is_empty());
        let wrong_root = parse(r#"{"allowedChannelPlugins":null}"#);
        assert_eq!(
            wrong_root
                .normalized_projection
                .as_ref()
                .unwrap()
                .array_len("/allowedChannelPlugins"),
            Some(0)
        );
    }

    #[test]
    fn source_oracle_worktree_lock_preserves_valid_options_and_substitutes_fail_closed() {
        let valid = parse(
            r#"{"worktree":{"symlinkDirectories":["node_modules"],"sparsePaths":["crates/core"],"baseRef":"head","bgIsolation":"none","location":"~/worktrees"}}"#,
        );
        let normalized = valid.normalized_projection.as_ref().unwrap();
        assert_eq!(normalized.string_at("/worktree/baseRef"), Some("head"));
        assert_eq!(normalized.string_at("/worktree/bgIsolation"), Some("none"));
        assert_eq!(
            normalized.string_at("/worktree/location"),
            Some("~/worktrees")
        );
        assert_eq!(
            normalized.array_len("/worktree/symlinkDirectories"),
            Some(1)
        );
        assert_eq!(normalized.array_len("/worktree/sparsePaths"), Some(1));
        assert!(valid.unresolved_fields.is_empty());

        let invalid_lock = parse(
            r#"{"worktree":{"baseRef":"unknown","bgIsolation":"none","location":17,"ignored":true},"model":"kept"}"#,
        );
        let normalized = invalid_lock.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/worktree/bgIsolation"),
            Some("worktree")
        );
        assert!(normalized.at_pointer("/worktree/baseRef").is_none());
        assert!(normalized.at_pointer("/worktree/location").is_none());
        assert!(normalized.at_pointer("/worktree/ignored").is_none());
        assert_eq!(normalized.string_at("/model"), Some("kept"));
        assert_eq!(invalid_lock.substituted_paths, vec!["/worktree"]);
        assert_eq!(invalid_lock.synthesized_paths, vec!["/worktree"]);

        let invalid_bg_isolation = parse(r#"{"worktree":{"bgIsolation":"none-ish"}}"#);
        let normalized = invalid_bg_isolation.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/worktree/bgIsolation"),
            Some("worktree")
        );
        assert_eq!(
            invalid_bg_isolation.substituted_paths,
            vec!["/worktree/bgIsolation"]
        );
    }

    #[test]
    fn source_oracle_root_admission_and_unresolved_schema_are_separate() {
        let result = parse(r#"{"managedMcpServers":[]}"#);
        assert!(result.document_admitted);
        assert_eq!(result.status, NativePolicyParseStatus::ParsedObject);
        assert!(result
            .unresolved_fields
            .contains(&"managedMcpServers".to_owned()));
        assert!(result.document_has_policy_content.is_none());

        let non_object = parse("[]");
        assert!(!non_object.document_admitted);
        assert_eq!(
            non_object.status,
            NativePolicyParseStatus::RejectedNonObjectRoot
        );
    }

    #[test]
    fn source_oracle_policy_parser_does_not_confuse_user_writable_with_field_filtering() {
        let result = parse_policy_document(
            r#"{"managedMcpServers":{"x":{}},"isolation":{"required":true},"prependPlugins":["x"],"enabledPlugins":{"user@local":false,"user2@local":true,"sec@builtin":false}}"#,
        );
        let candidate = result.candidate_projection.as_ref().unwrap();
        assert!(candidate.contains_property("managedMcpServers"));
        assert!(candidate.contains_property("isolation"));
        assert_eq!(candidate.array_len("/prependPlugins"), Some(1));
        assert_eq!(
            candidate
                .at_pointer("/enabledPlugins/user@local")
                .and_then(JsonValueView::as_bool),
            Some(false),
        );
        assert_eq!(
            candidate
                .at_pointer("/enabledPlugins/user2@local")
                .and_then(JsonValueView::as_bool),
            Some(true)
        );
        assert_eq!(
            candidate
                .at_pointer("/enabledPlugins/sec@builtin")
                .and_then(JsonValueView::as_bool),
            Some(false)
        );
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

    #[test]
    fn policy_helper_parsers_keep_utf16_and_mark_native_startup_fatal_paths() {
        let valid = parse(
            r#"{"policyHelper":{"path":"/opt/helper","timeoutMs":1200,"refreshIntervalMs":0},"policyHelpers":{"linux":{"path":"/usr/bin/helper-\ud800","onFailure":"continue","retries":7,"timeoutMs":9999999999,"refreshIntervalMs":60000,"outputBehavior":"merge","ignored":true}}}"#,
        );
        let normalized = valid.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized.string_at("/policyHelper/path"),
            Some("/opt/helper")
        );
        assert!(normalized
            .at_pointer("/policyHelpers/linux/ignored")
            .is_none());
        assert_eq!(
            normalized.string_units("/policyHelpers/linux/path"),
            Some(vec![
                b'/' as u16,
                b'u' as u16,
                b's' as u16,
                b'r' as u16,
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
        assert_eq!(
            normalized
                .at_pointer("/policyHelpers/linux/retries")
                .and_then(JsonValueView::as_number),
            Some(5.0)
        );
        assert_eq!(
            normalized
                .at_pointer("/policyHelpers/linux/timeoutMs")
                .and_then(JsonValueView::as_number),
            Some(2_147_483_647.0)
        );
        assert_eq!(valid.startup_fatal_paths, Vec::<String>::new());
        assert!(valid.unresolved_fields.is_empty());

        let null_singular = parse(r#"{"policyHelper":null,"model":"kept"}"#);
        assert!(null_singular
            .normalized_projection
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelper")
            .is_none());
        assert!(null_singular.unresolved_fields.is_empty());

        let bad_static = parse(
            r#"{"policyHelpers":{"linux":{"path":"/usr/bin/helper","defaultSettings":{"policyHelper":{"path":"/bad"}}}}}"#,
        );
        assert_eq!(
            bad_static.startup_fatal_paths,
            vec!["policyHelpers.linux.defaultSettings".to_owned()]
        );
        assert_eq!(
            bad_static.startup_fatal_diagnostics[0]
                .message
                .as_ref()
                .and_then(|message| message.at_pointer("").and_then(JsonValueView::as_str)),
            Some("\"policyHelpers.linux.defaultSettings\" is not a valid static settings payload: must not contain \"policyHelper\" — the default payload is applied as managed settings and cannot configure further policy helpers. When delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed.")
        );
        assert_eq!(bad_static.unresolved_fields, Vec::<String>::new());
        assert!(bad_static
            .normalized_projection
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelpers/linux/defaultSettings")
            .is_none());

        let bad_map = parse(r#"{"policyHelpers":[],"model":"kept"}"#);
        assert_eq!(
            bad_map.startup_fatal_paths,
            vec!["policyHelpers".to_owned()]
        );
        assert_eq!(
            bad_map.startup_fatal_diagnostics[0]
                .message
                .as_ref()
                .and_then(|message| message.at_pointer("").and_then(JsonValueView::as_str)),
            Some("\"policyHelpers\" could not be parsed: expected an object mapping OS keys (macos, linux, windows, wsl) to helper entries, plus an optional \"default\" settings payload (Invalid input: expected object, received array). When delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed.")
        );
        assert_eq!(bad_map.unresolved_fields, Vec::<String>::new());
        assert!(bad_map
            .normalized_projection
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelpers")
            .is_none());

        let bad_entry = parse(r#"{"policyHelpers":{"linux":{"path":17,"onFailure":"refuse"}}}"#);
        assert_eq!(
            bad_entry.startup_fatal_paths,
            vec!["policyHelpers.linux".to_owned()]
        );
        assert_eq!(
            bad_entry.startup_fatal_diagnostics[0]
                .message
                .as_ref()
                .and_then(|message| message.string_at("")),
            Some("path: Invalid input: expected string, received number Its \"onFailure\" requires the helper, so when delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed.")
        );
        assert_eq!(bad_entry.unresolved_fields, Vec::<String>::new());

        let missing_entry = parse(r#"{"policyHelpers":{"linux":{"onFailure":"refuse"}}}"#);
        assert_eq!(
            missing_entry.startup_fatal_diagnostics[0]
                .message
                .as_ref()
                .and_then(|message| message.at_pointer("").and_then(JsonValueView::as_str)),
            Some("Entry must carry \"path\" (a helper executable) or \"script\" + \"interpreter\" (an inline helper), and/or \"defaultSettings\" (a static settings payload) Its \"onFailure\" requires the helper, so when delivered from an OS-admin policy source (MDM or the managed settings file), Claude Code will not start until this is fixed.")
        );

        let schema_order = parse(
            r#"{"policyHelpers":{"default":{"path":"/bad"},"wsl":{"defaultSettings":{"path":"/bad"}},"windows":{"defaultSettings":{"path":"/bad"}},"linux":{"defaultSettings":{"path":"/bad"}},"macos":{"defaultSettings":{"path":"/bad"}}}}"#,
        );
        assert_eq!(
            schema_order.startup_fatal_paths,
            [
                "policyHelpers.macos.defaultSettings",
                "policyHelpers.linux.defaultSettings",
                "policyHelpers.windows.defaultSettings",
                "policyHelpers.wsl.defaultSettings",
                "policyHelpers.default"
            ]
            .map(str::to_owned)
        );

        let removed_map = parse(r#"{"policyHelpers":null,"model":"kept"}"#);
        assert!(removed_map.startup_fatal_paths.is_empty());
        assert!(removed_map
            .normalized_projection
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelpers")
            .is_none());

        let wsl_fallback = parse(
            r#"{"policyHelpers":{"wsl":{"path":17,"onFailure":"refuse"},"linux":{"defaultSettings":{"model":"fallback"}}}}"#,
        );
        assert!(wsl_fallback.startup_fatal_paths.is_empty());
        assert_eq!(
            wsl_fallback
                .normalized_projection
                .as_ref()
                .unwrap()
                .string_at("/policyHelpers/linux/defaultSettings/model"),
            Some("fallback")
        );
        assert!(wsl_fallback.unresolved_fields.is_empty());
    }

    #[test]
    fn policy_helper_static_payload_passes_i5e_without_a_parser_placeholder() {
        let result = parse(r#"{"policyHelpers":{"linux":{"defaultSettings":{"model":"kept"}}}}"#);
        assert!(result.unresolved_fields.is_empty());
        assert!(result.startup_fatal_paths.is_empty());
        assert_eq!(
            result
                .normalized_projection
                .as_ref()
                .unwrap()
                .string_at("/policyHelpers/linux/defaultSettings/model"),
            Some("kept")
        );
    }

    #[test]
    fn invalid_helper_config_keeps_valid_static_fallback_for_full_validation() {
        let result = parse(
            r#"{"policyHelpers":{"linux":{"path":17,"onFailure":"refuse","outputBehavior":"merge","defaultSettings":{"model":"kept"}}}}"#,
        );
        assert!(result.unresolved_fields.is_empty());
        assert!(result.startup_fatal_paths.is_empty());
        let normalized = result.normalized_projection.as_ref().unwrap();
        assert!(normalized.at_pointer("/policyHelpers/linux/path").is_none());
        assert_eq!(
            normalized.string_at("/policyHelpers/linux/outputBehavior"),
            Some("merge")
        );
        assert_eq!(
            normalized.string_at("/policyHelpers/linux/defaultSettings/model"),
            Some("kept")
        );
        assert!(result.helper_diagnostics.iter().any(|row| row.message.string_at("") == Some(
            "Invalid entry: its helper fields were ignored (path: Invalid input: expected string, received number), but its \"defaultSettings\" static payload was kept. No policy helper runs on linux from this entry."
        )));
    }

    #[test]
    fn helper_zod_diagnostics_use_source_order_first_three_and_status_rules() {
        let invalid = parse(
            r#"{"policyHelpers":{"linux":{"timeoutMs":"bad","refreshIntervalMs":500,"path":17,"script":false,"interpreter":"other","outputBehavior":"other","retries":-1,"unknown":"stripped"}}}"#,
        );
        let errors = invalid
            .helper_diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.path == "policyHelpers.linux")
            .map(|diagnostic| {
                diagnostic
                    .message
                    .string_at("")
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0],
            "Invalid entry was ignored: timeoutMs: Invalid input: expected number, received string; refreshIntervalMs: Invalid input; path: Invalid input: expected string, received number. No policy helper runs on linux from this entry."
        );
        assert!(invalid.startup_fatal_paths.is_empty());
        assert!(invalid
            .normalized_projection
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelpers/unknown")
            .is_none());

        let scalar_entry = parse(r#"{"policyHelpers":{"linux":17}}"#);
        let scalar_diagnostic = scalar_entry
            .helper_diagnostics
            .iter()
            .find(|diagnostic| diagnostic.path == "policyHelpers.linux")
            .unwrap();
        assert_eq!(
            scalar_diagnostic.message.string_at(""),
            Some("Invalid entry was ignored: Invalid input: expected object, received number. No policy helper runs on linux from this entry.")
        );
        assert!(scalar_diagnostic.status_only);
        assert!(scalar_entry.startup_fatal_paths.is_empty());

        let missing_singular_path = parse(r#"{"policyHelper":{}}"#);
        assert_eq!(
            missing_singular_path.helper_diagnostics[0]
                .message
                .string_at(""),
            Some("Invalid input: expected string, received undefined. This field was ignored.")
        );
        assert!(missing_singular_path.helper_diagnostics[0].status_only);

        let null_singular_path = parse(r#"{"policyHelper":{"path":null}}"#);
        assert_eq!(
            null_singular_path.helper_diagnostics[0]
                .message
                .string_at(""),
            Some("Invalid input: expected string, received null. This field was ignored.")
        );
        assert!(null_singular_path.helper_diagnostics[0].status_only);

        let null_on_failure =
            parse(r#"{"policyHelpers":{"linux":{"path":"/usr/bin/helper","onFailure":null}}}"#);
        assert!(!null_on_failure
            .helper_diagnostics
            .iter()
            .any(|diagnostic| diagnostic.path == "policyHelpers.linux.onFailure"));
        assert!(null_on_failure
            .normalized_projection
            .as_ref()
            .unwrap()
            .at_pointer("/policyHelpers/linux/onFailure")
            .is_none());

        let invalid_on_failure =
            parse(r#"{"policyHelpers":{"linux":{"path":"/usr/bin/helper","onFailure":"💡"}}}"#);
        assert_eq!(
            invalid_on_failure
                .normalized_projection
                .as_ref()
                .unwrap()
                .string_at("/policyHelpers/linux/onFailure"),
            Some("refuse")
        );
        let warning = invalid_on_failure
            .helper_diagnostics
            .iter()
            .find(|diagnostic| diagnostic.path == "policyHelpers.linux.onFailure")
            .unwrap();
        assert!(warning.status_only);
        assert_eq!(
            warning.message.string_at(""),
            Some("\"onFailure\": \"?\" on the policyHelpers.linux entry is not a recognized value (\"continue\" or \"refuse\"); it is treated as \"refuse\" — a startup failure of this entry's helper with no static payload in its place will not start Claude Code.")
        );
    }

    #[test]
    fn helper_path_and_script_constraints_preserve_native_messages() {
        let result = parse(
            r#"{"policyHelpers":{"linux":{"path":"relative","script":"\u0000","interpreter":"sh"}}}"#,
        );
        let entry = result
            .helper_diagnostics
            .iter()
            .find(|diagnostic| diagnostic.path == "policyHelpers.linux")
            .unwrap();
        assert_eq!(
            entry.message.string_at(""),
            Some("Invalid entry was ignored: path: path must be absolute; script: script must not contain NUL bytes. No policy helper runs on linux from this entry.")
        );

        let normalized = parse(
            r#"{"policyHelpers":{"linux":{"path":"/usr/bin/helper","timeoutMs":2147483648,"retries":999,"onFailure":"continue"}}}"#,
        );
        let normalized = normalized.normalized_projection.as_ref().unwrap();
        assert_eq!(
            normalized
                .at_pointer("/policyHelpers/linux/timeoutMs")
                .and_then(JsonValueView::as_number),
            Some(2_147_483_647.0)
        );
        assert_eq!(
            normalized
                .at_pointer("/policyHelpers/linux/retries")
                .and_then(JsonValueView::as_number),
            Some(5.0)
        );
    }

    #[test]
    fn native_i5e_record_admission_has_exact_forbidden_key_order_and_null_semantics() {
        let invalid_roots = [
            ("null", "Invalid input: expected record, received null"),
            ("[]", "Invalid input: expected record, received array"),
            ("\"x\"", "Invalid input: expected record, received string"),
            ("true", "Invalid input: expected record, received boolean"),
            ("1e999", "Invalid input: expected record, received Infinity"),
        ];
        for (raw, expected) in invalid_roots {
            let projection = Utf16JsonProjection::parse(raw).unwrap();
            assert_eq!(
                static_helper_settings_issue(&projection).as_deref(),
                Some(expected)
            );
        }

        let first_issue = Utf16JsonProjection::parse(
            r#"{"default":false,"path":"/bad","policyHelpers":{},"policyHelper":{}}"#,
        )
        .unwrap();
        assert_eq!(
            static_helper_settings_issue(&first_issue).as_deref(),
            Some("must not contain \"policyHelper\" — the default payload is applied as managed settings and cannot configure further policy helpers")
        );

        let entry_field_issue = Utf16JsonProjection::parse(r#"{"defaultSettings":{}}"#).unwrap();
        assert_eq!(
            static_helper_settings_issue(&entry_field_issue).as_deref(),
            Some("must not contain \"defaultSettings\" — a static payload is a managed-settings object, not a policyHelpers entry; entry fields (path/script/interpreter/outputBehavior/onFailure/retries/timeoutMs/refreshIntervalMs/defaultSettings) belong on the per-OS entries (policyHelpers.macos/linux/windows/wsl)")
        );

        let allowed_nulls = Utf16JsonProjection::parse(
            r#"{"policyHelper":null,"policyHelpers":null,"path":null,"script":null,"interpreter":null,"outputBehavior":null,"onFailure":null,"retries":null,"timeoutMs":null,"refreshIntervalMs":null,"defaultSettings":null,"managedSettings":null,"appendSystemPrompt":null,"macos":null,"linux":null,"windows":null,"wsl":null,"default":null}"#,
        )
        .unwrap();
        assert_eq!(static_helper_settings_issue(&allowed_nulls), None);
    }
}
