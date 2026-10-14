//! Acceptance-only source port of Native 2.1.295 `At → k_` for
//! `extraKnownMarketplaces`.
//!
//! Source is fixed to binary SHA-256
//! `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70` and
//! raw chunk `src_187673145.js` SHA-256
//! `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`.
//! `At` calls `k_` at local 380323 / binary 188053468. `k_` calls the
//! `Go().safeParse` schema at local 192564; `Go.source` is `m0t() → Ki(false)`
//! at local 148277. The validation, reserved-name warning, and error
//! formatting helpers are source-bound in the adjacent oracle receipt.
//!
//! The At and Due functions are separate phases. The `k_` At transform keeps
//! successful rows byte-for-byte as projections; `Go.safeParse` data is not
//! written back by `k_`. `validate_native_extra_known_marketplaces_due`
//! applies the final `Record<string, Go>().check(...)` parse, strips unknown
//! object keys from successful rows, and checks settings source names against
//! their dynamic record keys. Unknown top-level source discriminators and
//! settings-plugin unknown source discriminators are deliberately retained
//! without an At warning, matching `b_`/`E_`; Due then accepts or rejects them.
//! `R5e` is exported by this source chunk but has no call edge from `k_` or
//! `fTr`; it is a separate source-normalization function, so this At helper
//! does not rewrite URL credentials or provenance.

use super::native_policy_js_semantics::{
    NativePolicyJsSemanticsRegistry, NativePolicyZodUrlOutcome,
};
use crate::types::utf16_json::{JsonObjectEntryView, JsonValueView, Utf16JsonProjection};

/// Collect URL-constructor inputs reached by the Native `k_` declaration
/// parser and its nested settings-plugin source branches. It follows only
/// recognized discriminators and schema-typed string fields.
pub(crate) fn collect_native_extra_marketplace_url_inputs(
    source: &Utf16JsonProjection,
    url_inputs: &mut Vec<Vec<u16>>,
) {
    let Ok(declarations) = source.subprojection("/extraKnownMarketplaces") else {
        return;
    };
    let Some(root) = declarations.at_pointer("") else {
        return;
    };
    let Some(entries) = js_ordered_entries(root) else {
        return;
    };
    for declaration_entry in entries {
        let Ok(declaration) =
            declarations.subprojection_property_units(declaration_entry.code_units)
        else {
            continue;
        };
        let Some(declaration_root) = declaration.at_pointer("") else {
            continue;
        };
        if declaration_root.object_entries().is_none() {
            continue;
        }
        let Some(source_view) = declaration_root.get_property("source") else {
            continue;
        };
        if source_view.object_entries().is_none() {
            continue;
        }
        let Some(kind) = source_view
            .get_property("source")
            .and_then(JsonValueView::as_str)
        else {
            continue;
        };
        match kind {
            "url" => {
                if let Some(url) = source_view
                    .get_property("url")
                    .and_then(JsonValueView::code_units)
                {
                    super::native_policy::NativePolicySemanticInputs::push_unique(
                        url_inputs,
                        url.to_vec(),
                    );
                }
            }
            "npm" => {
                if let Some(registry) = source_view
                    .get_property("registry")
                    .and_then(JsonValueView::code_units)
                {
                    super::native_policy::NativePolicySemanticInputs::push_unique(
                        url_inputs,
                        registry.to_vec(),
                    );
                }
            }
            "settings" => {
                let Some(plugins) = source_view.get_property("plugins") else {
                    continue;
                };
                let Some(plugin_count) = plugins.array_len() else {
                    continue;
                };
                for index in 0..plugin_count {
                    let Some(plugin) = plugins.get_index(index) else {
                        continue;
                    };
                    if plugin.object_entries().is_none() {
                        continue;
                    }
                    let Some(plugin_source) = plugin.get_property("source") else {
                        continue;
                    };
                    if plugin_source.object_entries().is_none() {
                        continue;
                    }
                    match plugin_source
                        .get_property("source")
                        .and_then(JsonValueView::as_str)
                    {
                        Some("npm") => {
                            if let Some(registry) = plugin_source
                                .get_property("registry")
                                .and_then(JsonValueView::code_units)
                            {
                                super::native_policy::NativePolicySemanticInputs::push_unique(
                                    url_inputs,
                                    registry.to_vec(),
                                );
                            }
                        }
                        Some("archive") => {
                            if let Some(url) = plugin_source
                                .get_property("url")
                                .and_then(JsonValueView::code_units)
                            {
                                super::native_policy::NativePolicySemanticInputs::push_unique(
                                    url_inputs,
                                    url.to_vec(),
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

const FIELD: &str = "extraKnownMarketplaces";
const RESERVED_OFFICIAL_MESSAGE: &str =
    "Marketplace name impersonates an official Anthropic/Claude marketplace";
const RESERVED_MARKETPLACE_WARNING: &str = "Marketplace entry was ignored: its name reads like an official Anthropic or Claude marketplace. It stays in this file: rename it (the key and the source's name) to use it, or delete it. If /plugin also shows a marketplace of this name as refused, remove that one in /plugin → Marketplaces once this entry is renamed or deleted.";
const REMOTE_SETTINGS_PLUGIN_WARNING: &str = "Plugins in a settings-sourced marketplace must use remote sources (github, git-subdir, npm, url, archive, command). Relative-path sources like \"./foo\" have no marketplace repository to resolve against.";
const UNSUPPORTED_SETTINGS_PLUGIN_WARNING: &str = "source.source: 'unsupported' is a parse-time placeholder and cannot be authored. Use a remote source (github, git-subdir, npm, url, archive, command).";
const HEADERS_HELPER_PRINTABLE: &str = "headersHelper must be printable ASCII (letters, digits, punctuation, single spaces) with no runs of 4 or more spaces";
const HEADERS_HELPER_LONG: &str =
    "headersHelper must not be longer than the install consent UI can display";
const ARCHIVE_URL_RESTRICTED: &str =
    "Archive URLs must use https:// and must not point at a loopback, link-local, or cloud-metadata host";
const RESERVED_SETTINGS_SOURCE_MESSAGE: &str = "Reserved marketplace names cannot be used with settings sources. validateOfficialNameSource only accepts github/git sources from anthropics/* for these names; a settings source would be rejected after loadAndCacheMarketplace has already written to disk with cleanupNeeded=false.";
const MAX_HEADERS_HELPER_UNITS: usize = 500;
const MAX_COMMAND_TIMEOUT: f64 = 600.0;
const TOP_SOURCE_TYPES: &[&str] = &[
    "url",
    "github",
    "git",
    "npm",
    "file",
    "directory",
    "skills-dir",
    "hostPattern",
    "pathPattern",
    "settings",
];
const PLUGIN_SOURCE_TYPES: &[&str] = &[
    "npm",
    "url",
    "github",
    "git-subdir",
    "archive",
    "command",
    "unsupported",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeExtraMarketplaceSeverity {
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeExtraMarketplaceDiagnostic {
    pub(crate) path: Utf16JsonProjection,
    pub(crate) message: Utf16JsonProjection,
    pub(crate) severity: NativeExtraMarketplaceSeverity,
    pub(crate) invalid_value: Option<Utf16JsonProjection>,
    pub(crate) preserve_on_write: bool,
    pub(crate) raw_warning: Utf16JsonProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeExtraKnownMarketplacesResult {
    /// Post-`k_` field value. `None` means absent or deleted; `Some({})` is a
    /// present map even when every invalid row was removed.
    pub(crate) normalized: Option<Utf16JsonProjection>,
    pub(crate) diagnostics: Vec<NativeExtraMarketplaceDiagnostic>,
    pub(crate) unresolved: bool,
}

/// Due's `Record<string, Go>().check(...)` result for the post-At
/// `extraKnownMarketplaces` map. `normalized` is present only when the full
/// record schema and its settings-name/key check succeed, matching Zod's
/// `safeParse` data contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeExtraKnownMarketplacesDueResult {
    pub(crate) normalized: Option<Utf16JsonProjection>,
    pub(crate) issues: Vec<NativeExtraKnownMarketplacesDueIssue>,
    pub(crate) unresolved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeExtraKnownMarketplacesDueIssue {
    /// Zod path array, preserving arbitrary marketplace keys as UTF-16.
    pub(crate) path: Utf16JsonProjection,
    /// Zod issue message as exact UTF-16, including dynamic key/name text.
    pub(crate) message: Utf16JsonProjection,
}

#[must_use]
pub(crate) fn normalize_native_extra_known_marketplaces(
    root: &Utf16JsonProjection,
    file: &str,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> NativeExtraKnownMarketplacesResult {
    let Some(root_view) = root.at_pointer("") else {
        return empty_result();
    };
    if root_view.object_entries().is_none() {
        // `k_` is a no-op unless its input is an object.
        return empty_result();
    }
    let Some(field_view) = root_view.get_property(FIELD) else {
        return NativeExtraKnownMarketplacesResult {
            normalized: None,
            diagnostics: Vec::new(),
            unresolved: false,
        };
    };
    let Ok(field) = root.subprojection_property_units(&ascii_units(FIELD)) else {
        return unresolved(None, Vec::new());
    };

    let Some(entries) = js_ordered_entries(field_view) else {
        let received = native_type(field_view);
        let message = format!(
            "\"{FIELD}\" must be an object mapping marketplace names to declarations; received {received}. This field was ignored."
        );
        let mut diagnostics = Vec::new();
        emit_warning(
            &mut diagnostics,
            file,
            ascii_units(FIELD),
            ascii_units(&message),
            Some(string_projection(&received)),
            false,
        );
        return NativeExtraKnownMarketplacesResult {
            normalized: None,
            diagnostics,
            unresolved: false,
        };
    };

    let mut kept = Vec::with_capacity(entries.len());
    let mut diagnostics = Vec::new();
    let mut unresolved = false;

    for entry in entries {
        let Ok(value) = field.subprojection_property_units(entry.code_units) else {
            unresolved = true;
            continue;
        };
        let Some(value_view) = value.at_pointer("") else {
            unresolved = true;
            continue;
        };

        if should_keep_unknown_source(value_view) {
            kept.push((entry.code_units.to_vec(), value));
            continue;
        }

        match validate_marketplace_declaration(&value, semantics) {
            DeclarationValidation::Unresolved => {
                unresolved = true;
                kept.push((entry.code_units.to_vec(), value));
            }
            DeclarationValidation::Invalid(issues) => {
                let raw_settings_name = settings_source_name(value_view);
                let lookalike = !issues.is_empty()
                    && issues
                        .iter()
                        .all(|issue| issue.message == ascii_units(RESERVED_OFFICIAL_MESSAGE))
                    && raw_settings_name.as_deref() == Some(entry.code_units)
                    && raw_settings_name
                        .as_deref()
                        .is_some_and(is_native_reserved_lookalike);
                let path = marketplace_path(entry.code_units);
                let message = if lookalike {
                    ascii_units(RESERVED_MARKETPLACE_WARNING)
                } else {
                    let details = format_native_issues(&issues);
                    ascii_units(&format!("Invalid marketplace entry was ignored: {details}"))
                };
                emit_warning(&mut diagnostics, file, path, message, None, lookalike);
            }
            DeclarationValidation::Valid { settings_name } => {
                if let Some(settings_name) = settings_name {
                    if settings_name != entry.code_units {
                        let key = safe_native_key(entry.code_units);
                        let source_name = safe_native_key(&settings_name);
                        let message = format!(
                            "Invalid marketplace entry was ignored: key \"{key}\" must match the settings source name \"{source_name}\""
                        );
                        emit_warning(
                            &mut diagnostics,
                            file,
                            marketplace_path(entry.code_units),
                            ascii_units(&message),
                            None,
                            false,
                        );
                        continue;
                    }
                }
                // `k_` validates with `safeParse` but leaves the original row
                // in the map; final Due parsing owns unknown-key stripping.
                kept.push((entry.code_units.to_vec(), value));
            }
        }
    }

    if unresolved {
        return NativeExtraKnownMarketplacesResult {
            normalized: Some(field),
            diagnostics,
            unresolved: true,
        };
    }

    match Utf16JsonProjection::object_units(kept) {
        Ok(normalized) => NativeExtraKnownMarketplacesResult {
            normalized: Some(normalized),
            diagnostics,
            unresolved: false,
        },
        Err(_) => NativeExtraKnownMarketplacesResult {
            normalized: Some(field),
            diagnostics,
            unresolved: true,
        },
    }
}

/// Apply the final Due record validator and its settings source-name check to
/// the field value produced by `normalize_native_extra_known_marketplaces`.
///
/// At `k_` filters declarations, including settings name/key mismatches, and
/// retains the original successful rows. This separate Due phase parses each
/// surviving `Go` value again, strips unknown object properties, and applies
/// the record-level settings name/key check to any row that reaches it.
#[must_use]
pub(crate) fn validate_native_extra_known_marketplaces_due(
    value: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> NativeExtraKnownMarketplacesDueResult {
    let Some(root) = value.at_pointer("") else {
        return due_unresolved();
    };
    let Some(entries) = js_ordered_entries(root) else {
        return NativeExtraKnownMarketplacesDueResult {
            normalized: None,
            issues: vec![due_issue(
                vec![string_projection(FIELD)],
                invalid_type_message("record", native_type(root)),
            )],
            unresolved: false,
        };
    };

    let mut parsed_entries = Vec::with_capacity(entries.len());
    let mut issues = Vec::new();
    let mut unresolved = false;

    // Record value schemas run in JavaScript Object.entries order. Collect all
    // Go issues before the record-level `.check`, which does not execute when
    // any value parse aborts.
    for entry in entries {
        let Ok(declaration) = value.subprojection_property_units(entry.code_units) else {
            unresolved = true;
            continue;
        };
        match validate_marketplace_declaration(&declaration, semantics) {
            DeclarationValidation::Valid { settings_name } => {
                parsed_entries.push((entry.code_units.to_vec(), declaration, settings_name));
            }
            DeclarationValidation::Invalid(go_issues) => {
                for issue in go_issues {
                    let Some(issue) = due_go_issue(entry.code_units, issue) else {
                        unresolved = true;
                        continue;
                    };
                    issues.push(issue);
                }
            }
            DeclarationValidation::Unresolved => unresolved = true,
        }
    }

    if unresolved {
        return NativeExtraKnownMarketplacesDueResult {
            normalized: None,
            issues,
            unresolved: true,
        };
    }
    if !issues.is_empty() {
        return NativeExtraKnownMarketplacesDueResult {
            normalized: None,
            issues,
            unresolved: false,
        };
    }

    // `s(i)`'s check runs after every Go value has parsed. It walks the
    // normalized record using Object.entries order and emits a custom issue
    // for each settings source whose name differs from its key.
    for (key, _, settings_name) in &parsed_entries {
        if let Some(settings_name) = settings_name {
            if settings_name.as_slice() != key.as_slice() {
                issues.push(due_settings_name_mismatch_issue(key, settings_name));
            }
        }
    }
    if !issues.is_empty() {
        return NativeExtraKnownMarketplacesDueResult {
            normalized: None,
            issues,
            unresolved: false,
        };
    }

    let mut normalized_entries = Vec::with_capacity(parsed_entries.len());
    for (key, declaration, _) in parsed_entries {
        match normalize_marketplace_declaration(&declaration, semantics) {
            Ok(normalized) => normalized_entries.push((key, normalized)),
            Err(()) => return due_unresolved(),
        }
    }

    match Utf16JsonProjection::object_units(normalized_entries) {
        Ok(normalized) => NativeExtraKnownMarketplacesDueResult {
            normalized: Some(normalized),
            issues: Vec::new(),
            unresolved: false,
        },
        Err(_) => due_unresolved(),
    }
}

enum DeclarationValidation {
    Valid { settings_name: Option<Vec<u16>> },
    Invalid(Vec<GoIssue>),
    Unresolved,
}

#[derive(Clone)]
struct GoIssue {
    path: Vec<PathPart>,
    message: Vec<u16>,
    unrecognized_keys: Option<Vec<Vec<u16>>>,
    continuable: bool,
    unresolved: bool,
}

#[derive(Clone)]
enum PathPart {
    Key(Vec<u16>),
    Index(usize),
}

fn validate_marketplace_declaration(
    source: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> DeclarationValidation {
    let Some(root) = source.at_pointer("") else {
        return DeclarationValidation::Unresolved;
    };
    let Some(_) = root.object_entries() else {
        return DeclarationValidation::Invalid(vec![GoIssue::new(
            Vec::new(),
            invalid_type_message("object", native_type(root)),
        )]);
    };

    let mut issues = Vec::new();
    let mut settings_name = None;
    match root.get_property("source") {
        None => issues.push(GoIssue::new(
            vec![PathPart::Key(ascii_units("source"))],
            invalid_type_message("object", "undefined"),
        )),
        Some(source_view) if source_view.object_entries().is_none() => {
            issues.push(GoIssue::new(
                vec![PathPart::Key(ascii_units("source"))],
                invalid_type_message("object", native_type(source_view)),
            ));
        }
        Some(source_view) => {
            let Ok(source_object) = source.subprojection_property_units(&ascii_units("source"))
            else {
                return DeclarationValidation::Unresolved;
            };
            let Some(source_object_view) = source_object.at_pointer("") else {
                return DeclarationValidation::Unresolved;
            };
            let discriminator = source_view
                .get_property("source")
                .and_then(JsonValueView::code_units);
            if let Some(discriminator) = discriminator {
                let source_type = TOP_SOURCE_TYPES
                    .iter()
                    .copied()
                    .find(|kind| ascii_eq(discriminator, kind));
                if let Some(source_type) = source_type {
                    validate_source_variant(
                        &source_object,
                        source_object_view,
                        source_type,
                        &mut issues,
                        &mut settings_name,
                        semantics,
                    );
                } else {
                    let expected = format!(
                        "Invalid discriminator value. Expected {}",
                        quoted_options(TOP_SOURCE_TYPES)
                    );
                    issues.push(GoIssue::new(
                        vec![
                            PathPart::Key(ascii_units("source")),
                            PathPart::Key(ascii_units("source")),
                        ],
                        ascii_units(&expected),
                    ));
                }
            } else {
                let expected = format!(
                    "Invalid discriminator value. Expected {}",
                    quoted_options(TOP_SOURCE_TYPES)
                );
                issues.push(GoIssue::new(
                    vec![
                        PathPart::Key(ascii_units("source")),
                        PathPart::Key(ascii_units("source")),
                    ],
                    ascii_units(&expected),
                ));
            }
        }
    }

    validate_optional_string_field(source, root, "installLocation", &mut issues, &[]);
    validate_optional_bool_field(source, root, "autoUpdate", &mut issues, &[]);

    if issues.iter().any(|issue| issue.unresolved) {
        DeclarationValidation::Unresolved
    } else if issues.is_empty() {
        DeclarationValidation::Valid { settings_name }
    } else {
        DeclarationValidation::Invalid(issues)
    }
}

impl GoIssue {
    fn new(path: Vec<PathPart>, message: Vec<u16>) -> Self {
        Self {
            path,
            message,
            unrecognized_keys: None,
            continuable: false,
            unresolved: false,
        }
    }

    fn dirty(path: Vec<PathPart>, message: Vec<u16>) -> Self {
        Self {
            path,
            message,
            unrecognized_keys: None,
            continuable: true,
            unresolved: false,
        }
    }

    fn unresolved(path: Vec<PathPart>) -> Self {
        Self {
            path,
            message: Vec::new(),
            unrecognized_keys: None,
            continuable: false,
            unresolved: true,
        }
    }
}

fn validate_source_variant(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    source_type: &str,
    issues: &mut Vec<GoIssue>,
    settings_name: &mut Option<Vec<u16>>,
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let prefix = vec![PathPart::Key(ascii_units("source"))];
    match source_type {
        "url" => {
            validate_required_url_field(source, root, "url", issues, &prefix, semantics);
            validate_optional_string_record(source, root, "headers", issues, &prefix);
            validate_optional_headers_helper(source, root, "headersHelper", issues, &prefix);
        }
        "github" => {
            validate_required_string_field(source, root, "repo", issues, &prefix);
            validate_optional_string_field(source, root, "ref", issues, &prefix);
            validate_optional_string_field(source, root, "path", issues, &prefix);
            validate_optional_string_array(source, root, "sparsePaths", issues, &prefix);
            validate_optional_bool_field(source, root, "skipLfs", issues, &prefix);
        }
        "git" => {
            validate_required_string_field(source, root, "url", issues, &prefix);
            validate_optional_string_field(source, root, "ref", issues, &prefix);
            validate_optional_string_field(source, root, "path", issues, &prefix);
            validate_optional_string_array(source, root, "sparsePaths", issues, &prefix);
            validate_optional_bool_field(source, root, "skipLfs", issues, &prefix);
        }
        "npm" => {
            validate_npm_package(source, root, "package", issues, &prefix, false);
            validate_optional_string_field(source, root, "version", issues, &prefix);
            validate_optional_registry_url(
                source, root, "registry", issues, &prefix, true, semantics,
            );
        }
        "file" | "directory" => {
            validate_required_string_field(source, root, "path", issues, &prefix);
        }
        "skills-dir" => {}
        "hostPattern" => {
            validate_required_string_field(source, root, "hostPattern", issues, &prefix);
        }
        "pathPattern" => {
            validate_required_string_field(source, root, "pathPattern", issues, &prefix);
        }
        "settings" => {
            *settings_name = validate_marketplace_name(source, root, issues, &prefix);
            validate_settings_plugins(source, root, issues, &prefix, semantics);
            validate_optional_owner(source, root, issues, &prefix);
        }
        _ => unreachable!("source type is from TOP_SOURCE_TYPES"),
    }
}

fn validate_marketplace_name(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) -> Option<Vec<u16>> {
    let name_path = path_with_key(prefix, "name");
    let Some(name) = root.get_property("name") else {
        issues.push(GoIssue::new(
            name_path,
            invalid_type_message("string", "undefined"),
        ));
        return None;
    };
    let Some(units) = name.code_units() else {
        issues.push(GoIssue::new(
            name_path,
            invalid_type_message("string", native_type(name)),
        ));
        return None;
    };

    if units.is_empty() {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units("Marketplace must have a name"),
        ));
    }
    if units.contains(&u16::from(b' ')) {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units(
                "Marketplace name cannot contain spaces. Use kebab-case (e.g., \"my-marketplace\")",
            ),
        ));
    }
    if contains_marketplace_name_control(units) {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units(
                "Marketplace name cannot contain control or bidirectional-formatting characters",
            ),
        ));
    }
    if units == [u16::from(b'.')]
        || units.contains(&u16::from(b'/'))
        || units.contains(&u16::from(b'\\'))
        || contains_pair(units, b"..")
    {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units("Marketplace name cannot contain path separators (/ or \\\\), \"..\" sequences, or be \".\""),
        ));
    }
    if name_impersonates_official_marketplace(units) {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units(RESERVED_OFFICIAL_MESSAGE),
        ));
    }

    let lowercase = js_lowercase_ascii_output(units);
    if let Some(lowercase) = lowercase {
        if built_in_marketplace_reason(&lowercase).is_some()
            && !is_official_marketplace_name(&lowercase)
        {
            let message = format!(
                "Marketplace name \"{}\" is reserved for {}",
                String::from_utf16_lossy(&lowercase),
                built_in_marketplace_reason(&lowercase).unwrap_or_default()
            );
            issues.push(GoIssue::new(name_path.clone(), ascii_units(&message)));
        }
        if reserved_marketplace_names()
            .iter()
            .any(|reserved| ascii_eq(&lowercase, reserved))
        {
            issues.push(GoIssue::new(
                name_path,
                ascii_units(RESERVED_SETTINGS_SOURCE_MESSAGE),
            ));
        }
    }
    if let Ok(value) = source.subprojection_property_units(&ascii_units("name")) {
        Some(value.at_pointer("")?.code_units()?.to_vec())
    } else {
        None
    }
}

fn validate_settings_plugins(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let field_path = path_with_key(prefix, "plugins");
    let Some(plugins_view) = root.get_property("plugins") else {
        issues.push(GoIssue::new(
            field_path,
            invalid_type_message("array", "undefined"),
        ));
        return;
    };
    let Some(length) = plugins_view.array_len() else {
        issues.push(GoIssue::new(
            field_path,
            invalid_type_message("array", native_type(plugins_view)),
        ));
        return;
    };
    let Ok(plugins) = source.subprojection_property_units(&ascii_units("plugins")) else {
        return;
    };
    for index in 0..length {
        let Ok(plugin) = plugins.subprojection(&format!("/{index}")) else {
            continue;
        };
        validate_settings_plugin(&plugin, index, issues, prefix, semantics);
    }
}

fn validate_settings_plugin(
    plugin: &Utf16JsonProjection,
    index: usize,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let mut plugin_path = path_with_key(prefix, "plugins");
    plugin_path.push(PathPart::Index(index));
    let Some(root) = plugin.at_pointer("") else {
        return;
    };
    if root.object_entries().is_none() {
        issues.push(GoIssue::new(
            plugin_path,
            invalid_type_message("object", native_type(root)),
        ));
        return;
    }

    validate_plugin_name(plugin, root, &plugin_path, issues);
    let source_result = validate_plugin_source(plugin, root, &plugin_path, issues, semantics);
    validate_optional_string_field(plugin, root, "description", issues, &plugin_path);
    validate_optional_string_field(plugin, root, "version", issues, &plugin_path);
    validate_optional_bool_field(plugin, root, "strict", issues, &plugin_path);
    validate_optional_string_record(plugin, root, "headers", issues, &plugin_path);
    validate_optional_headers_helper(plugin, root, "headersHelper", issues, &plugin_path);

    match source_result {
        PluginSourceResult::ValidStringPath => issues.push(GoIssue::new(
            plugin_path,
            ascii_units(REMOTE_SETTINGS_PLUGIN_WARNING),
        )),
        PluginSourceResult::ValidUnsupported => issues.push(GoIssue::new(
            plugin_path,
            ascii_units(UNSUPPORTED_SETTINGS_PLUGIN_WARNING),
        )),
        PluginSourceResult::ValidObject
        | PluginSourceResult::Invalid
        | PluginSourceResult::Missing => {}
    }
}

enum PluginSourceResult {
    ValidObject,
    ValidStringPath,
    ValidUnsupported,
    Invalid,
    Missing,
}

fn validate_plugin_name(
    plugin: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    path: &[PathPart],
    issues: &mut Vec<GoIssue>,
) {
    let name_path = path_with_key(path, "name");
    let Some(name) = root.get_property("name") else {
        issues.push(GoIssue::new(
            name_path,
            invalid_type_message("string", "undefined"),
        ));
        return;
    };
    let Some(units) = name.code_units() else {
        issues.push(GoIssue::new(
            name_path,
            invalid_type_message("string", native_type(name)),
        ));
        return;
    };
    if units.is_empty() {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units("Plugin name cannot be empty"),
        ));
    }
    if units.contains(&u16::from(b' ')) {
        issues.push(GoIssue::new(
            name_path.clone(),
            ascii_units("Plugin name cannot contain spaces. Use kebab-case (e.g., \"my-plugin\")"),
        ));
    }
    if contains_plugin_name_control(units) {
        issues.push(GoIssue::new(
            name_path,
            ascii_units(
                "Plugin name cannot contain control or bidirectional-formatting characters",
            ),
        ));
    }
    let _ = plugin;
}

fn validate_plugin_source(
    plugin: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    path: &[PathPart],
    issues: &mut Vec<GoIssue>,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> PluginSourceResult {
    let source_path = path_with_key(path, "source");
    let Some(source_view) = root.get_property("source") else {
        issues.push(GoIssue::new(
            source_path,
            invalid_type_message("object", "undefined"),
        ));
        return PluginSourceResult::Missing;
    };
    if let Some(units) = source_view.code_units() {
        // `Bi` also has a local-path string branch, but `wg` refuses all
        // string sources for inline settings marketplaces.
        if units == [u16::from(b'.')]
            || (units.len() >= 2 && units[0] == u16::from(b'.') && units[1] == u16::from(b'/'))
        {
            return PluginSourceResult::ValidStringPath;
        }
        issues.push(GoIssue::new(source_path, ascii_units("Invalid input")));
        return PluginSourceResult::Invalid;
    }
    let Some(source_root) = source_view.object_entries() else {
        issues.push(GoIssue::new(
            source_path,
            invalid_type_message("object", native_type(source_view)),
        ));
        return PluginSourceResult::Invalid;
    };
    let _ = source_root;
    let Ok(source_projection) = plugin.subprojection_property_units(&ascii_units("source")) else {
        return PluginSourceResult::Invalid;
    };
    let Some(source_fields) = source_projection.at_pointer("") else {
        return PluginSourceResult::Invalid;
    };
    let Some(kind_units) = source_fields
        .get_property("source")
        .and_then(JsonValueView::code_units)
    else {
        issues.push(GoIssue::new(source_path, ascii_units("Invalid input")));
        return PluginSourceResult::Invalid;
    };
    let Some(kind) = PLUGIN_SOURCE_TYPES
        .iter()
        .copied()
        .find(|kind| ascii_eq(kind_units, kind))
    else {
        issues.push(GoIssue::new(source_path, ascii_units("Invalid input")));
        return PluginSourceResult::Invalid;
    };

    let mut nested_issues = Vec::new();
    match kind {
        "npm" => {
            validate_nested_npm_source(
                &source_projection,
                source_fields,
                &mut nested_issues,
                &source_path,
                semantics,
            );
        }
        "url" => {
            validate_required_string_field(
                &source_projection,
                source_fields,
                "url",
                &mut nested_issues,
                &source_path,
            );
            validate_optional_string_field(
                &source_projection,
                source_fields,
                "ref",
                &mut nested_issues,
                &source_path,
            );
            validate_optional_sha_field(
                &source_projection,
                source_fields,
                "sha",
                40,
                &mut nested_issues,
                &source_path,
            );
        }
        "github" => {
            validate_required_string_field(
                &source_projection,
                source_fields,
                "repo",
                &mut nested_issues,
                &source_path,
            );
            validate_optional_string_field(
                &source_projection,
                source_fields,
                "ref",
                &mut nested_issues,
                &source_path,
            );
            validate_optional_sha_field(
                &source_projection,
                source_fields,
                "sha",
                40,
                &mut nested_issues,
                &source_path,
            );
        }
        "git-subdir" => {
            validate_required_string_field(
                &source_projection,
                source_fields,
                "url",
                &mut nested_issues,
                &source_path,
            );
            validate_required_nonempty_string_field(
                &source_projection,
                source_fields,
                "path",
                &mut nested_issues,
                &source_path,
            );
            validate_optional_string_field(
                &source_projection,
                source_fields,
                "ref",
                &mut nested_issues,
                &source_path,
            );
            validate_optional_sha_field(
                &source_projection,
                source_fields,
                "sha",
                40,
                &mut nested_issues,
                &source_path,
            );
        }
        "archive" => {
            validate_archive_source(
                &source_projection,
                source_fields,
                &mut nested_issues,
                &source_path,
                semantics,
            );
        }
        "command" => {
            validate_command_source(
                &source_projection,
                source_fields,
                &mut nested_issues,
                &source_path,
            );
        }
        "unsupported" => {
            validate_optional_string_field(
                &source_projection,
                source_fields,
                "error",
                &mut nested_issues,
                &source_path,
            );
        }
        _ => unreachable!("source kind came from PLUGIN_SOURCE_TYPES"),
    }

    if !nested_issues.is_empty() {
        // `Bi()` is an ordinary union. If the selected branch aborts (for
        // example, a missing required field or an invalid nested npm package
        // union), Zod collapses it to `source: Invalid input`. A dirty branch
        // such as archive or registry URL refinements keeps its field issues.
        if nested_issues.iter().any(|issue| !issue.continuable) {
            issues.push(GoIssue::new(source_path, ascii_units("Invalid input")));
        } else {
            issues.extend(nested_issues);
        }
        return PluginSourceResult::Invalid;
    }
    if kind == "unsupported" {
        return PluginSourceResult::ValidUnsupported;
    }

    PluginSourceResult::ValidObject
}

fn validate_nested_npm_source(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    issues: &mut Vec<GoIssue>,
    path: &[PathPart],
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    validate_npm_package(source, root, "package", issues, path, true);
    validate_optional_string_field(source, root, "version", issues, path);
    validate_optional_registry_url(source, root, "registry", issues, path, false, semantics);
}

fn validate_archive_source(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    issues: &mut Vec<GoIssue>,
    path: &[PathPart],
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let url_path = path_with_key(path, "url");
    let Some(value) = root.get_property("url") else {
        issues.push(GoIssue::new(
            url_path,
            invalid_type_message("string", "undefined"),
        ));
        return;
    };
    let Some(units) = value.code_units() else {
        issues.push(GoIssue::new(
            url_path,
            invalid_type_message("string", native_type(value)),
        ));
        return;
    };
    let Some(row) = semantics.url(units) else {
        issues.push(GoIssue::unresolved(url_path));
        return;
    };
    let NativePolicyZodUrlOutcome::Valid {
        archive_zod_refine_valid,
        ..
    } = &row.zod_url
    else {
        issues.push(GoIssue::dirty(url_path.clone(), ascii_units("Invalid URL")));
        if !row.raw_archive_refine_valid {
            issues.push(GoIssue::dirty(
                url_path.clone(),
                ascii_units(ARCHIVE_URL_RESTRICTED),
            ));
        }
        validate_optional_sha_field(source, root, "sha256", 64, issues, path);
        return;
    };
    if !archive_zod_refine_valid {
        issues.push(GoIssue::dirty(
            url_path.clone(),
            ascii_units(ARCHIVE_URL_RESTRICTED),
        ));
    }
    validate_optional_sha_field(source, root, "sha256", 64, issues, path);
}

fn validate_command_source(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    issues: &mut Vec<GoIssue>,
    path: &[PathPart],
) {
    let command_path = path_with_key(path, "command");
    match root.get_property("command") {
        None => issues.push(GoIssue::new(
            command_path,
            invalid_type_message("string", "undefined"),
        )),
        Some(value) if value.code_units().is_none() => issues.push(GoIssue::new(
            command_path,
            invalid_type_message("string", native_type(value)),
        )),
        Some(value) => {
            let units = value.code_units().unwrap_or_default();
            if units.is_empty() {
                issues.push(GoIssue::dirty(
                    command_path.clone(),
                    ascii_units("Too small: expected string to have >=1 characters"),
                ));
            }
            if units.len() > MAX_HEADERS_HELPER_UNITS {
                issues.push(GoIssue::dirty(
                    command_path.clone(),
                    ascii_units(
                        "command must not be longer than the install consent UI can display",
                    ),
                ));
            }
            if !is_printable_ascii_command(units) {
                issues.push(GoIssue::dirty(
                    command_path,
                    ascii_units("command must be printable ASCII (letters, digits, punctuation, single spaces) with no runs of 4 or more spaces"),
                ));
            }
        }
    }
    if let Some(timeout) = root.get_property("timeout") {
        let timeout_path = path_with_key(path, "timeout");
        match timeout.as_number() {
            Some(number) => {
                if number.fract() != 0.0 {
                    issues.push(GoIssue::dirty(
                        timeout_path.clone(),
                        ascii_units("Invalid number: expected integer"),
                    ));
                }
                if number <= 0.0 {
                    issues.push(GoIssue::dirty(
                        timeout_path.clone(),
                        ascii_units("Too small: expected number to be >0"),
                    ));
                }
                if number > MAX_COMMAND_TIMEOUT {
                    issues.push(GoIssue::dirty(
                        timeout_path,
                        ascii_units("Too big: expected number to be <=600"),
                    ));
                }
            }
            None => issues.push(GoIssue::new(
                timeout_path,
                invalid_type_message("number", native_type(timeout)),
            )),
        }
    }
    if let Some(mode) = root.get_property("mode") {
        let mode_path = path_with_key(path, "mode");
        let valid = mode
            .code_units()
            .is_some_and(|units| ascii_eq(units, "copy") || ascii_eq(units, "link"));
        if !valid {
            if mode.code_units().is_none() {
                issues.push(GoIssue::new(
                    mode_path,
                    invalid_type_message("string", native_type(mode)),
                ));
            } else {
                issues.push(GoIssue::new(
                    mode_path,
                    ascii_units("Invalid option: expected one of \"copy\"|\"link\""),
                ));
            }
        }
    }
    let _ = source;
}

fn validate_settings_owner(
    owner: &Utf16JsonProjection,
    issues: &mut Vec<GoIssue>,
    path: &[PathPart],
) {
    let Some(root) = owner.at_pointer("") else {
        return;
    };
    let Some(_) = root.object_entries() else {
        issues.push(GoIssue::new(
            path.to_vec(),
            invalid_type_message("object", native_type(root)),
        ));
        return;
    };
    match root.get_property("name") {
        None => issues.push(GoIssue::new(
            path_with_key(path, "name"),
            invalid_type_message("string", "undefined"),
        )),
        Some(value) if value.code_units().is_none() => issues.push(GoIssue::new(
            path_with_key(path, "name"),
            invalid_type_message("string", native_type(value)),
        )),
        Some(value) if value.code_units().is_some_and(<[u16]>::is_empty) => {
            issues.push(GoIssue::new(
                path_with_key(path, "name"),
                ascii_units("Author name cannot be empty"),
            ))
        }
        Some(_) => {}
    }
    for key in ["email", "url"] {
        if let Some(value) = root.get_property(key) {
            if value.code_units().is_none() {
                issues.push(GoIssue::new(
                    path_with_key(path, key),
                    invalid_type_message("string", native_type(value)),
                ));
            }
        }
    }
}

fn validate_optional_owner(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    if root.get_property("owner").is_none() {
        return;
    }
    let path = path_with_key(prefix, "owner");
    let Ok(owner) = source.subprojection_property_units(&ascii_units("owner")) else {
        return;
    };
    validate_settings_owner(&owner, issues, &path);
}

fn validate_required_url_field(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let path = path_with_key(prefix, key);
    let Some(value) = root.get_property(key) else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", "undefined"),
        ));
        return;
    };
    let Some(units) = value.code_units() else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", native_type(value)),
        ));
        return;
    };
    match semantics.url(units) {
        Some(row) if matches!(row.zod_url, NativePolicyZodUrlOutcome::Valid { .. }) => {}
        Some(_) => issues.push(GoIssue::new(path, ascii_units("Invalid URL"))),
        None => issues.push(GoIssue::unresolved(path)),
    }
    let _ = source;
}

fn validate_optional_registry_url(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
    has_zod_url: bool,
    semantics: &NativePolicyJsSemanticsRegistry,
) {
    let Some(value) = root.get_property(key) else {
        return;
    };
    let path = path_with_key(prefix, key);
    let Some(units) = value.code_units() else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", native_type(value)),
        ));
        return;
    };
    let Some(row) = semantics.url(units) else {
        issues.push(GoIssue::unresolved(path));
        return;
    };
    let (url_valid, registry_valid) = if has_zod_url {
        match &row.zod_url {
            NativePolicyZodUrlOutcome::Invalid => (false, row.raw_rdt_valid),
            NativePolicyZodUrlOutcome::Valid {
                rdt_zod_value_valid,
                ..
            } => (true, *rdt_zod_value_valid),
        }
    } else {
        (true, row.raw_rdt_valid)
    };
    if has_zod_url && !url_valid {
        issues.push(GoIssue::dirty(path.clone(), ascii_units("Invalid URL")));
    }
    if !registry_valid {
        issues.push(GoIssue::dirty(
            path,
            ascii_units("Registry must be an http(s) URL"),
        ));
    }
    let _ = source;
}

fn validate_npm_package(
    _source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
    plugin_package: bool,
) {
    let path = path_with_key(prefix, key);
    let Some(value) = root.get_property(key) else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", "undefined"),
        ));
        return;
    };
    let Some(units) = value.code_units() else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", native_type(value)),
        ));
        return;
    };
    let valid = if plugin_package {
        is_npm_package_or_reference(units)
    } else {
        is_npm_package(units) || is_npm_scope_wildcard(units)
    };
    if !valid {
        issues.push(GoIssue::new(
            path,
            ascii_units(if plugin_package {
                "Package reference cannot contain \"..\" path segments"
            } else {
                "Invalid input"
            }),
        ));
    }
}

fn validate_optional_headers_helper(
    _source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    let Some(value) = root.get_property(key) else {
        return;
    };
    let path = path_with_key(prefix, key);
    let Some(units) = value.code_units() else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", native_type(value)),
        ));
        return;
    };
    if units.len() > MAX_HEADERS_HELPER_UNITS {
        issues.push(GoIssue::new(path.clone(), ascii_units(HEADERS_HELPER_LONG)));
    }
    if !is_printable_ascii_command(units) {
        issues.push(GoIssue::new(path, ascii_units(HEADERS_HELPER_PRINTABLE)));
    }
}

fn validate_optional_string_field(
    _source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    if let Some(value) = root.get_property(key) {
        if value.code_units().is_none() {
            issues.push(GoIssue::new(
                path_with_key(prefix, key),
                invalid_type_message("string", native_type(value)),
            ));
        }
    }
}

fn validate_required_string_field(
    _source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    match root.get_property(key) {
        None => issues.push(GoIssue::new(
            path_with_key(prefix, key),
            invalid_type_message("string", "undefined"),
        )),
        Some(value) if value.code_units().is_none() => issues.push(GoIssue::new(
            path_with_key(prefix, key),
            invalid_type_message("string", native_type(value)),
        )),
        Some(_) => {}
    }
}

fn validate_required_nonempty_string_field(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    validate_required_string_field(source, root, key, issues, prefix);
    if root
        .get_property(key)
        .and_then(JsonValueView::code_units)
        .is_some_and(<[u16]>::is_empty)
    {
        issues.push(GoIssue::dirty(
            path_with_key(prefix, key),
            ascii_units("Too small: expected string to have >=1 characters"),
        ));
    }
}

fn validate_optional_bool_field(
    _source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    if let Some(value) = root.get_property(key) {
        if value.as_bool().is_none() {
            issues.push(GoIssue::new(
                path_with_key(prefix, key),
                invalid_type_message("boolean", native_type(value)),
            ));
        }
    }
}

fn validate_optional_string_array(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    let Some(value) = root.get_property(key) else {
        return;
    };
    let path = path_with_key(prefix, key);
    let Some(length) = value.array_len() else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("array", native_type(value)),
        ));
        return;
    };
    let Ok(array) = source.subprojection_property_units(&ascii_units(key)) else {
        return;
    };
    for index in 0..length {
        if let Some(item) = value.get_index(index) {
            if item.code_units().is_none() {
                issues.push(GoIssue::new(
                    path_with_index(&path, index),
                    invalid_type_message("string", native_type(item)),
                ));
            }
        } else {
            let _ = &array;
        }
    }
}

fn validate_optional_string_record(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    let Some(value) = root.get_property(key) else {
        return;
    };
    let path = path_with_key(prefix, key);
    let Some(entries) = js_ordered_entries(value) else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("record", native_type(value)),
        ));
        return;
    };
    let Ok(record) = source.subprojection_property_units(&ascii_units(key)) else {
        return;
    };
    for entry in entries {
        if ascii_eq(entry.code_units, "__proto__") {
            continue;
        }
        if entry.value.code_units().is_none() {
            issues.push(GoIssue::new(
                path_with_key_units(&path, entry.code_units),
                invalid_type_message("string", native_type(entry.value)),
            ));
        }
    }
    let _ = record;
}

fn validate_optional_sha_field(
    _source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    length: usize,
    issues: &mut Vec<GoIssue>,
    prefix: &[PathPart],
) {
    let Some(value) = root.get_property(key) else {
        return;
    };
    let path = path_with_key(prefix, key);
    let Some(units) = value.code_units() else {
        issues.push(GoIssue::new(
            path,
            invalid_type_message("string", native_type(value)),
        ));
        return;
    };
    if length == 40 {
        if units.len() != 40 {
            issues.push(GoIssue::dirty(
                path.clone(),
                ascii_units("Invalid length: expected string to have 40 characters"),
            ));
        }
        let valid = units.iter().all(|unit| {
            (u16::from(b'0')..=u16::from(b'9')).contains(unit)
                || (u16::from(b'a')..=u16::from(b'f')).contains(unit)
        });
        if !valid {
            issues.push(GoIssue::dirty(
                path,
                ascii_units("Must be a full 40-character lowercase git commit SHA"),
            ));
        }
    } else {
        let valid = units.len() == 64
            && units.iter().all(|unit| {
                (u16::from(b'0')..=u16::from(b'9')).contains(unit)
                    || (u16::from(b'a')..=u16::from(b'f')).contains(unit)
                    || (u16::from(b'A')..=u16::from(b'F')).contains(unit)
            });
        if !valid {
            issues.push(GoIssue::dirty(
                path,
                ascii_units("Must be a 64-character hex SHA-256 digest"),
            ));
        }
    }
}

fn should_keep_unknown_source(value: JsonValueView<'_>) -> bool {
    if let Some(source_type) = value
        .get_property("source")
        .and_then(|source| source.get_property("source"))
        .and_then(JsonValueView::code_units)
    {
        if !TOP_SOURCE_TYPES
            .iter()
            .any(|kind| ascii_eq(source_type, kind))
        {
            return true;
        }
    }
    let Some(plugins) = value
        .get_property("source")
        .and_then(|source| source.get_property("plugins"))
    else {
        return false;
    };
    let Some(length) = plugins.array_len() else {
        return false;
    };
    (0..length).any(|index| {
        plugins.get_index(index).is_some_and(|plugin| {
            plugin
                .get_property("source")
                .and_then(|source| source.get_property("source"))
                .and_then(JsonValueView::code_units)
                .is_some_and(|source_type| {
                    !PLUGIN_SOURCE_TYPES
                        .iter()
                        .any(|known| ascii_eq(source_type, known))
                })
        })
    })
}

fn settings_source_name(value: JsonValueView<'_>) -> Option<Vec<u16>> {
    let source = value.get_property("source")?;
    if !source
        .get_property("source")
        .and_then(JsonValueView::code_units)
        .is_some_and(|units| ascii_eq(units, "settings"))
    {
        return None;
    }
    source
        .get_property("name")
        .and_then(JsonValueView::code_units)
        .map(<[u16]>::to_vec)
}

fn is_native_reserved_lookalike(name: &[u16]) -> bool {
    name.iter().all(|unit| (0x20..=0x7e).contains(unit))
        && name_impersonates_official_marketplace(name)
}

fn name_impersonates_official_marketplace(name: &[u16]) -> bool {
    if reserved_marketplace_names().iter().any(|reserved| {
        js_lowercase_ascii_output(name).is_some_and(|lowercase| ascii_eq(&lowercase, reserved))
    }) {
        return false;
    }
    let Some(text) = ascii_string(name) else {
        // Native UAr matches every non-ASCII UTF-16 unit, so q$e returns true.
        return true;
    };
    let lower = text.to_ascii_lowercase();
    if lower
        .chars()
        .any(|character| !(0x20..=0x7e).contains(&(character as u32)))
    {
        return true;
    }

    let compact = lower
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect::<String>();
    if contains_pair_of_words(&lower, "official", &["anthropic", "claude"])
        || contains_pair_of_words(&lower, "anthropic", &["official"])
        || contains_pair_of_words(&lower, "claude", &["official"])
    {
        return true;
    }

    // `ui` is `(?:Ci|^(?:anthropic|claude)[^a-z0-9]*(marketplace|plugins|official|directory|directories))`.
    // The first alternative is covered above. This second alternative is
    // retried after Native strips leading punctuation.
    if starts_with_brand_suffix(&lower) {
        return true;
    }
    if [
        "officialanthropic",
        "officialclaude",
        "anthropicofficial",
        "claudeofficial",
    ]
    .iter()
    .any(|pattern| compact.contains(pattern))
    {
        return true;
    }

    let suffixes = [
        "marketplace",
        "plugins",
        "official",
        "directory",
        "directories",
    ];
    let tokens = lower
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();
    let max_joined_units = 9
        + suffixes
            .iter()
            .map(|suffix| suffix.len())
            .max()
            .unwrap_or(0)
        + 1;
    let mut joined = String::new();
    for token in tokens {
        joined.push_str(token);
        if joined.len() > max_joined_units {
            break;
        }
        if ["anthropic", "claude"].iter().any(|brand| {
            suffixes.iter().any(|suffix| {
                joined == format!("{brand}{suffix}") || joined == format!("{brand}{suffix}s")
            })
        }) {
            return true;
        }
    }
    false
}

fn contains_pair_of_words(text: &str, first: &str, seconds: &[&str]) -> bool {
    let bytes = text.as_bytes();
    for (start, _) in text.match_indices(first) {
        let after_first = start + first.len();
        for second in seconds {
            let mut cursor = after_first;
            while cursor < bytes.len() && !bytes[cursor].is_ascii_alphanumeric() {
                cursor += 1;
            }
            if text[cursor..].starts_with(second) {
                return true;
            }
        }
    }
    false
}

fn starts_with_brand_suffix(text: &str) -> bool {
    let trimmed = text.trim_start_matches(|character: char| !character.is_ascii_alphanumeric());
    ["anthropic", "claude"].iter().any(|brand| {
        trimmed.strip_prefix(brand).is_some_and(|remainder| {
            let suffix =
                remainder.trim_start_matches(|character: char| !character.is_ascii_alphanumeric());
            [
                "marketplace",
                "plugins",
                "official",
                "directory",
                "directories",
            ]
            .iter()
            .any(|word| suffix.starts_with(word))
        })
    })
}

fn reserved_marketplace_names() -> &'static [&'static str] {
    &[
        "claude-code-marketplace",
        "claude-code-plugins",
        "claude-plugins-official",
        "anthropic-marketplace",
        "anthropic-plugins",
        "agent-skills",
        "anthropic-agent-skills",
        "life-sciences",
        "knowledge-work-plugins",
        "claude-for-legal",
        "claude-for-financial-services",
        "financial-services-plugins",
        "first-party-plugins",
        "claude-tag-plugins",
        "claude-community",
        "claude-plugins-community",
        "healthcare",
        "anthropic-plugin-directory",
        "claude-plugin-directory",
    ]
}

fn built_in_marketplace_reason(name: &[u16]) -> Option<&'static str> {
    let name = ascii_string(name)?.to_ascii_lowercase();
    match name.as_str() {
        "inline" => Some("--plugin-dir session plugins"),
        "builtin" => Some("built-in plugins"),
        "skills-dir" => Some("plugins auto-loaded from .claude/skills/"),
        "synced" => Some("plugins synced from your claude.ai account"),
        "claude-plugin-test" => Some("plugins loaded by claude plugin test"),
        _ => None,
    }
}

fn is_official_marketplace_name(name: &[u16]) -> bool {
    ascii_string(name).is_some_and(|name| {
        ["npm", "pip", "uv", "cargo", "github", "gh"].contains(&name.to_ascii_lowercase().as_str())
    })
}

fn contains_marketplace_name_control(units: &[u16]) -> bool {
    units.iter().any(|unit| {
        (0x0000..=0x001f).contains(unit)
            || (0x007f..=0x009f).contains(unit)
            || matches!(*unit, 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069)
    })
}

fn contains_plugin_name_control(units: &[u16]) -> bool {
    contains_marketplace_name_control(units)
}

fn ascii_prefix_eq_ignore_case(units: &[u16], prefix: &str) -> bool {
    let expected = prefix.encode_utf16().collect::<Vec<_>>();
    units.len() >= expected.len()
        && units
            .iter()
            .zip(expected)
            .all(|(actual, expected)| ascii_lower_unit(*actual) == expected)
}

fn ascii_lower_unit(unit: u16) -> u16 {
    if (u16::from(b'A')..=u16::from(b'Z')).contains(&unit) {
        unit + (u16::from(b'a') - u16::from(b'A'))
    } else {
        unit
    }
}

fn is_printable_ascii_command(units: &[u16]) -> bool {
    let mut spaces = 0usize;
    for unit in units {
        if !(0x20..=0x7e).contains(unit) {
            return false;
        }
        if *unit == u16::from(b' ') {
            spaces += 1;
            if spaces >= 4 {
                return false;
            }
        } else {
            spaces = 0;
        }
    }
    true
}

fn is_npm_package(units: &[u16]) -> bool {
    if units.iter().any(|unit| *unit > 0x7f) {
        return false;
    }
    let Some(text) = ascii_string(units) else {
        return false;
    };
    if text.contains("..") || text.contains("//") {
        return false;
    }
    let bytes = text.as_bytes();
    if bytes.first() == Some(&b'@') {
        let Some(slash) = bytes.iter().position(|byte| *byte == b'/') else {
            return false;
        };
        slash > 1
            && slash + 1 < bytes.len()
            && npm_name_body(&bytes[1..slash])
            && npm_name_body(&bytes[slash + 1..])
    } else {
        npm_name_body(bytes)
    }
}

fn is_npm_scope_wildcard(units: &[u16]) -> bool {
    if units.iter().any(|unit| *unit > 0x7f) {
        return false;
    }
    let Some(text) = ascii_string(units) else {
        return false;
    };
    if text.contains("..") || text.contains("//") || !text.ends_with("/*") {
        return false;
    }
    let scope = &text[..text.len() - 2];
    let Some(scope) = scope.strip_prefix('@') else {
        return false;
    };
    npm_name_body(scope.as_bytes())
}

fn is_npm_package_or_reference(units: &[u16]) -> bool {
    if is_npm_package(units) {
        return true;
    }
    let has_path_traversal = contains_pair(units, b"..");
    if !has_path_traversal {
        return true;
    }
    [
        "file:",
        "http:",
        "https:",
        "git:",
        "git+http:",
        "git+https:",
        "git+ssh:",
        "ssh:",
        "github:",
        "gitlab:",
        "bitbucket:",
    ]
    .iter()
    .any(|prefix| ascii_prefix_eq_ignore_case(units, prefix))
}

fn npm_name_body(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(*byte, b'-' | b'.' | b'_')
        })
}

fn emit_warning(
    diagnostics: &mut Vec<NativeExtraMarketplaceDiagnostic>,
    file: &str,
    path_units: Vec<u16>,
    message_units: Vec<u16>,
    invalid_value: Option<Utf16JsonProjection>,
    preserve_on_write: bool,
) {
    let path = string_projection_units(&path_units);
    let message = string_projection_units(&message_units);
    let mut fields = vec![
        (ascii_units("file"), string_projection(file)),
        (ascii_units("path"), path.clone()),
        (ascii_units("message"), message.clone()),
        (ascii_units("severity"), string_projection("warning")),
    ];
    if let Some(value) = &invalid_value {
        fields.push((ascii_units("invalidValue"), value.clone()));
    }
    if preserve_on_write {
        fields.push((ascii_units("preserveOnWrite"), bool_projection(true)));
    }
    let Ok(raw_warning) = Utf16JsonProjection::object_units(fields) else {
        return;
    };
    diagnostics.push(NativeExtraMarketplaceDiagnostic {
        path,
        message,
        severity: NativeExtraMarketplaceSeverity::Warning,
        invalid_value,
        preserve_on_write,
        raw_warning,
    });
}

fn format_native_issues(issues: &[GoIssue]) -> String {
    let mut formatted = Vec::new();
    for issue in issues.iter().take(3) {
        let path = native_issue_path(&issue.path);
        let message = if let Some(keys) = &issue.unrecognized_keys {
            format!(
                "Unrecognized {}: {}",
                if keys.len() == 1 { "field" } else { "fields" },
                keys.iter()
                    .map(|key| safe_native_key(key))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            native_error_text(&issue.message)
        };
        formatted.push(if path.is_empty() {
            message
        } else {
            format!("{path}: {message}")
        });
    }
    let more = issues.len().saturating_sub(formatted.len());
    if more > 0 {
        format!("{} (+{more} more)", formatted.join(", "))
    } else {
        formatted.join(", ")
    }
}

fn native_error_text(message: &[u16]) -> String {
    let mut text = String::from_utf16_lossy(message);
    text = text.replace("  ", " ").trim().to_owned();
    text = text.replace(['`', '｀', 'ˋ', '`', '‵'], "'");
    if text.chars().count() > 160 {
        let mut truncated = text.chars().take(159).collect::<String>();
        truncated.push('…');
        text = truncated;
    }
    text
}

fn native_issue_path(path: &[PathPart]) -> String {
    path.iter()
        .map(|part| match part {
            PathPart::Key(key) => safe_native_key(key),
            PathPart::Index(index) => index.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn marketplace_path(key: &[u16]) -> Vec<u16> {
    let mut path = ascii_units(FIELD);
    path.push(u16::from(b'.'));
    path.extend(ascii_units(&safe_native_key(key)));
    path
}

fn safe_native_key(units: &[u16]) -> String {
    let valid = !units.is_empty()
        && units.len() <= 40
        && units.iter().all(|unit| {
            (u16::from(b'A')..=u16::from(b'Z')).contains(unit)
                || (u16::from(b'a')..=u16::from(b'z')).contains(unit)
                || (u16::from(b'0')..=u16::from(b'9')).contains(unit)
                || matches!(*unit, 0x5f | 0x24 | 0x2e | 0x2d)
        });
    if valid {
        String::from_utf16_lossy(units)
    } else {
        "<key>".to_owned()
    }
}

fn native_type(value: JsonValueView<'_>) -> &'static str {
    if value.is_null() {
        "null"
    } else if value.code_units().is_some() {
        "string"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value.as_number().is_some() {
        "number"
    } else if value.array_len().is_some() {
        "array"
    } else if value.object_entries().is_some() {
        "object"
    } else {
        "undefined"
    }
}

fn invalid_type_message(expected: &str, received: &str) -> Vec<u16> {
    ascii_units(&format!(
        "Invalid input: expected {expected}, received {received}"
    ))
}

fn quoted_options(options: &[&str]) -> String {
    options
        .iter()
        .map(|option| format!("'{option}'"))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn path_with_key(path: &[PathPart], key: &str) -> Vec<PathPart> {
    let mut result = path.to_vec();
    result.push(PathPart::Key(ascii_units(key)));
    result
}

fn path_with_key_units(path: &[PathPart], key: &[u16]) -> Vec<PathPart> {
    let mut result = path.to_vec();
    result.push(PathPart::Key(key.to_vec()));
    result
}

fn path_with_index(path: &[PathPart], index: usize) -> Vec<PathPart> {
    let mut result = path.to_vec();
    result.push(PathPart::Index(index));
    result
}

fn js_ordered_entries<'a>(value: JsonValueView<'a>) -> Option<Vec<JsonObjectEntryView<'a>>> {
    let mut entries = value.object_entries()?;
    entries.sort_by(|left, right| {
        match (
            javascript_array_index(left.code_units),
            javascript_array_index(right.code_units),
        ) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    Some(entries)
}

fn javascript_array_index(units: &[u16]) -> Option<u32> {
    if units.is_empty() || units.len() > 10 || (units.len() > 1 && units[0] == u16::from(b'0')) {
        return None;
    }
    if units
        .iter()
        .any(|unit| !(u16::from(b'0')..=u16::from(b'9')).contains(unit))
    {
        return None;
    }
    let value = ascii_string(units)?.parse::<u32>().ok()?;
    (value < u32::MAX).then_some(value)
}

fn string_projection(text: &str) -> Utf16JsonProjection {
    string_projection_units(&ascii_units(text))
}

fn string_projection_units(units: &[u16]) -> Utf16JsonProjection {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
        .expect("display string matches exact source units")
}

fn bool_projection(value: bool) -> Utf16JsonProjection {
    Utf16JsonProjection::parse(if value { "true" } else { "false" })
        .expect("literal boolean is valid JSON")
}

fn ascii_units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

fn ascii_string(units: &[u16]) -> Option<String> {
    if units.iter().any(|unit| *unit > 0x7f) {
        return None;
    }
    Some(String::from_utf16_lossy(units))
}

fn ascii_eq(units: &[u16], text: &str) -> bool {
    units.iter().copied().eq(text.encode_utf16())
}

fn js_lowercase_ascii_output(units: &[u16]) -> Option<Vec<u16>> {
    let mut output = Vec::with_capacity(units.len());
    for unit in units {
        match *unit {
            0x0041..=0x005a => output.push(unit + 0x20),
            0x212a => output.push(u16::from(b'k')),
            0x0000..=0x007f => output.push(*unit),
            _ => return None,
        }
    }
    Some(output)
}

fn contains_pair(units: &[u16], needle: &[u8; 2]) -> bool {
    units
        .windows(2)
        .any(|pair| pair[0] == u16::from(needle[0]) && pair[1] == u16::from(needle[1]))
}

fn due_go_issue(key: &[u16], issue: GoIssue) -> Option<NativeExtraKnownMarketplacesDueIssue> {
    let mut path = vec![string_projection(FIELD), string_projection_units(key)];
    for part in issue.path {
        path.push(match part {
            PathPart::Key(units) => string_projection_units(&units),
            PathPart::Index(index) => Utf16JsonProjection::parse(&index.to_string()).ok()?,
        });
    }
    Some(due_issue(path, issue.message))
}

fn due_settings_name_mismatch_issue(
    key: &[u16],
    settings_name: &[u16],
) -> NativeExtraKnownMarketplacesDueIssue {
    let mut message = ascii_units(
        "Settings-sourced marketplace name must match its extraKnownMarketplaces key (got key \"",
    );
    message.extend_from_slice(key);
    message.extend(ascii_units("\" but source.name \""));
    message.extend_from_slice(settings_name);
    message.extend(ascii_units("\")"));
    due_issue(
        vec![
            string_projection(FIELD),
            string_projection_units(key),
            string_projection("source"),
            string_projection("name"),
        ],
        message,
    )
}

fn due_issue(
    path: Vec<Utf16JsonProjection>,
    message_units: Vec<u16>,
) -> NativeExtraKnownMarketplacesDueIssue {
    NativeExtraKnownMarketplacesDueIssue {
        path: Utf16JsonProjection::array(path)
            .expect("Due paths contain only valid JSON string/number projections"),
        message: string_projection_units(&message_units),
    }
}

fn due_unresolved() -> NativeExtraKnownMarketplacesDueResult {
    NativeExtraKnownMarketplacesDueResult {
        normalized: None,
        issues: Vec::new(),
        unresolved: true,
    }
}

fn normalize_marketplace_declaration(
    declaration: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Result<Utf16JsonProjection, ()> {
    let root = declaration.at_pointer("").ok_or(())?;
    let source_view = root.get_property("source").ok_or(())?;
    let source = declaration
        .subprojection_property_units(&ascii_units("source"))
        .map_err(|_| ())?;
    let source_root = source.at_pointer("").ok_or(())?;
    let kind = source_root
        .get_property("source")
        .and_then(JsonValueView::code_units)
        .ok_or(())?;
    let normalized_source =
        normalize_top_marketplace_source(&source, source_root, kind, semantics)?;

    let mut fields = vec![(ascii_units("source"), normalized_source)];
    push_present_property(declaration, root, "installLocation", &mut fields)?;
    push_present_property(declaration, root, "autoUpdate", &mut fields)?;
    let _ = source_view;
    Utf16JsonProjection::object_units(fields).map_err(|_| ())
}

fn normalize_top_marketplace_source(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    kind: &[u16],
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Result<Utf16JsonProjection, ()> {
    let fields: &[&str] = if ascii_eq(kind, "url") {
        &["source", "url", "headers", "headersHelper"]
    } else if ascii_eq(kind, "github") {
        &["source", "repo", "ref", "path", "sparsePaths", "skipLfs"]
    } else if ascii_eq(kind, "git") {
        &["source", "url", "ref", "path", "sparsePaths", "skipLfs"]
    } else if ascii_eq(kind, "npm") {
        &["source", "package", "version", "registry"]
    } else if ascii_eq(kind, "file") || ascii_eq(kind, "directory") {
        &["source", "path"]
    } else if ascii_eq(kind, "skills-dir") {
        &["source"]
    } else if ascii_eq(kind, "hostPattern") {
        &["source", "hostPattern"]
    } else if ascii_eq(kind, "pathPattern") {
        &["source", "pathPattern"]
    } else if ascii_eq(kind, "settings") {
        &["source", "name", "plugins", "owner"]
    } else {
        return Err(());
    };

    let mut output = Vec::with_capacity(fields.len());
    for field in fields {
        if *field == "plugins" {
            let plugins = normalize_settings_plugins(source, root, semantics)?;
            output.push((ascii_units(field), plugins));
        } else if *field == "owner" {
            if let Some(owner_view) = root.get_property(field) {
                let owner = source
                    .subprojection_property_units(&ascii_units(field))
                    .map_err(|_| ())?;
                output.push((
                    ascii_units(field),
                    normalize_settings_owner(&owner, owner_view)?,
                ));
            }
        } else if (ascii_eq(kind, "url") && *field == "url")
            || (ascii_eq(kind, "npm") && *field == "registry")
        {
            let normalized = normalize_zod_url_property(source, root, field, semantics)?;
            output.push((ascii_units(field), normalized));
        } else {
            push_present_property(source, root, field, &mut output)?;
        }
    }
    Utf16JsonProjection::object_units(output).map_err(|_| ())
}

fn normalize_settings_plugins(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Result<Utf16JsonProjection, ()> {
    let plugins_view = root.get_property("plugins").ok_or(())?;
    let length = plugins_view.array_len().ok_or(())?;
    let plugins = source
        .subprojection_property_units(&ascii_units("plugins"))
        .map_err(|_| ())?;
    let mut normalized = Vec::with_capacity(length);
    for index in 0..length {
        let plugin = plugins
            .subprojection(&format!("/{index}"))
            .map_err(|_| ())?;
        normalized.push(normalize_settings_plugin(&plugin, semantics)?);
    }
    Utf16JsonProjection::array(normalized).map_err(|_| ())
}

fn normalize_settings_plugin(
    plugin: &Utf16JsonProjection,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Result<Utf16JsonProjection, ()> {
    let root = plugin.at_pointer("").ok_or(())?;
    let source_view = root.get_property("source").ok_or(())?;
    let source = plugin
        .subprojection_property_units(&ascii_units("source"))
        .map_err(|_| ())?;
    let normalized_source = normalize_settings_plugin_source(&source, source_view, semantics)?;
    let mut output = vec![
        (
            ascii_units("name"),
            plugin
                .subprojection_property_units(&ascii_units("name"))
                .map_err(|_| ())?,
        ),
        (ascii_units("source"), normalized_source),
    ];
    for field in [
        "description",
        "version",
        "strict",
        "headers",
        "headersHelper",
    ] {
        push_present_property(plugin, root, field, &mut output)?;
    }
    Utf16JsonProjection::object_units(output).map_err(|_| ())
}

fn normalize_settings_plugin_source(
    source: &Utf16JsonProjection,
    source_view: JsonValueView<'_>,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Result<Utf16JsonProjection, ()> {
    if let Some(units) = source_view.code_units() {
        let normalized = if units == [u16::from(b'.')] {
            ascii_units("./")
        } else {
            units.to_vec()
        };
        return Ok(string_projection_units(&normalized));
    }

    let root = source.at_pointer("").ok_or(())?;
    let kind = root
        .get_property("source")
        .and_then(JsonValueView::code_units)
        .ok_or(())?;
    let fields: &[&str] = if ascii_eq(kind, "npm") {
        &["source", "package", "version", "registry"]
    } else if ascii_eq(kind, "url") {
        &["source", "url", "ref", "sha"]
    } else if ascii_eq(kind, "github") {
        &["source", "repo", "ref", "sha"]
    } else if ascii_eq(kind, "git-subdir") {
        &["source", "url", "path", "ref", "sha"]
    } else if ascii_eq(kind, "archive") {
        &["source", "url", "sha256"]
    } else if ascii_eq(kind, "command") {
        &["source", "command", "timeout", "mode"]
    } else if ascii_eq(kind, "unsupported") {
        &["source", "error"]
    } else {
        return Err(());
    };
    let mut output = Vec::with_capacity(fields.len());
    for field in fields {
        if ascii_eq(kind, "archive") && *field == "url" {
            output.push((
                ascii_units(field),
                normalize_zod_url_property(source, root, field, semantics)?,
            ));
        } else {
            push_present_property(source, root, field, &mut output)?;
        }
    }
    Utf16JsonProjection::object_units(output).map_err(|_| ())
}

fn normalize_zod_url_property(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    semantics: &NativePolicyJsSemanticsRegistry,
) -> Result<Utf16JsonProjection, ()> {
    let units = root
        .get_property(key)
        .and_then(JsonValueView::code_units)
        .ok_or(())?;
    let row = semantics.url(units).ok_or(())?;
    let NativePolicyZodUrlOutcome::Valid {
        zod_value_units, ..
    } = &row.zod_url
    else {
        return Err(());
    };
    let _ = source;
    Ok(string_projection_units(zod_value_units))
}

fn normalize_settings_owner(
    owner: &Utf16JsonProjection,
    root: JsonValueView<'_>,
) -> Result<Utf16JsonProjection, ()> {
    let mut output = Vec::with_capacity(3);
    push_present_property(owner, root, "name", &mut output)?;
    push_present_property(owner, root, "email", &mut output)?;
    push_present_property(owner, root, "url", &mut output)?;
    Utf16JsonProjection::object_units(output).map_err(|_| ())
}

fn push_present_property(
    source: &Utf16JsonProjection,
    root: JsonValueView<'_>,
    key: &str,
    output: &mut Vec<(Vec<u16>, Utf16JsonProjection)>,
) -> Result<(), ()> {
    if root.get_property(key).is_some() {
        output.push((
            ascii_units(key),
            source
                .subprojection_property_units(&ascii_units(key))
                .map_err(|_| ())?,
        ));
    }
    Ok(())
}

fn unresolved(
    normalized: Option<Utf16JsonProjection>,
    diagnostics: Vec<NativeExtraMarketplaceDiagnostic>,
) -> NativeExtraKnownMarketplacesResult {
    NativeExtraKnownMarketplacesResult {
        normalized,
        diagnostics,
        unresolved: true,
    }
}

fn empty_result() -> NativeExtraKnownMarketplacesResult {
    NativeExtraKnownMarketplacesResult {
        normalized: None,
        diagnostics: Vec::new(),
        unresolved: false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        normalize_native_extra_known_marketplaces as normalize_native_extra_known_marketplaces_source,
        validate_native_extra_known_marketplaces_due as validate_native_extra_known_marketplaces_due_source,
    };
    use crate::settings::native_policy_js_semantics::{
        NativePolicyDisplayRow, NativePolicyJsSemanticsRegistry, NativePolicyUrlSemanticRow,
        NativePolicyZodUrlOutcome,
    };
    use crate::types::utf16_json::Utf16JsonProjection;

    fn parse(source: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(source).expect("fixture is valid JSON")
    }

    fn test_semantics(source: &Utf16JsonProjection) -> NativePolicyJsSemanticsRegistry {
        let mut inputs = Vec::<Vec<u16>>::new();
        let Some(root) = source.at_pointer("") else {
            return empty_semantics();
        };
        let mut stack = vec![root];
        while let Some(value) = stack.pop() {
            if let Some(entries) = value.object_entries() {
                for entry in entries {
                    if ["url", "registry"]
                        .iter()
                        .any(|key| entry.code_units.iter().copied().eq(key.encode_utf16()))
                    {
                        if let Some(units) = entry.value.code_units() {
                            if !inputs.iter().any(|old| old.as_slice() == units) {
                                inputs.push(units.to_vec());
                            }
                        }
                    }
                    stack.push(entry.value);
                }
            } else if let Some(length) = value.array_len() {
                for index in 0..length {
                    if let Some(item) = value.get_index(index) {
                        stack.push(item);
                    }
                }
            }
        }
        let url_rows = inputs.into_iter().map(test_url_row).collect::<Vec<_>>();
        NativePolicyJsSemanticsRegistry::from_rows(url_rows, Vec::<NativePolicyDisplayRow>::new())
            .expect("test semantic input keys are unique")
    }

    fn test_url_row(input_units: Vec<u16>) -> NativePolicyUrlSemanticRow {
        let text = String::from_utf16_lossy(&input_units);
        let parsed = url::Url::parse(&text).ok();
        let rdt_raw_valid = parsed.as_ref().is_some_and(|url| {
            let prefix = if text
                .get(..7)
                .is_some_and(|value| value.eq_ignore_ascii_case("http://"))
            {
                7
            } else if text
                .get(..8)
                .is_some_and(|value| value.eq_ignore_ascii_case("https://"))
            {
                8
            } else {
                return false;
            };
            text.as_bytes()
                .get(prefix)
                .is_some_and(|byte| !matches!(*byte, b'/' | b'\\'))
                && matches!(url.scheme(), "http" | "https")
        });
        let archive_ok = parsed.as_ref().is_some_and(|url| {
            url.scheme() == "https"
                && url.host_str().is_some_and(|host| {
                    !matches!(
                        host.to_ascii_lowercase().as_str(),
                        "localhost" | "127.0.0.1" | "::1"
                    )
                })
        });
        let zod_url = parsed
            .as_ref()
            .map_or(NativePolicyZodUrlOutcome::Invalid, |url| {
                let host = url.host_str().unwrap_or_default().to_owned();
                let port = url.port().map(|port| port.to_string()).unwrap_or_default();
                let protocol = format!("{}:", url.scheme());
                NativePolicyZodUrlOutcome::Valid {
                    zod_value_units: input_units.clone(),
                    href_units: url.as_str().encode_utf16().collect(),
                    protocol_units: protocol.encode_utf16().collect(),
                    username_units: url.username().encode_utf16().collect(),
                    password_units: url.password().unwrap_or_default().encode_utf16().collect(),
                    host_units: host.encode_utf16().collect(),
                    hostname_units: host.encode_utf16().collect(),
                    port_units: port.encode_utf16().collect(),
                    pathname_units: url.path().encode_utf16().collect(),
                    search_units: url.query().map_or_else(Vec::new, |value| {
                        format!("?{value}").encode_utf16().collect()
                    }),
                    hash_units: url.fragment().map_or_else(Vec::new, |value| {
                        format!("#{value}").encode_utf16().collect()
                    }),
                    origin_units: url.origin().ascii_serialization().encode_utf16().collect(),
                    rdt_zod_value_valid: rdt_raw_valid,
                    archive_zod_refine_valid: archive_ok,
                    archive_zod_hostname_blocked: Some(!archive_ok),
                }
            });
        let archive_reached = parsed.as_ref().is_some_and(|url| url.scheme() == "https");
        NativePolicyUrlSemanticRow {
            input_units,
            raw_rdt_valid: rdt_raw_valid,
            raw_archive_refine_valid: archive_ok,
            raw_archive_hostname_blocked: archive_reached.then_some(!archive_ok),
            zod_url,
        }
    }

    fn empty_semantics() -> NativePolicyJsSemanticsRegistry {
        NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
            .expect("empty test semantic input set")
    }

    fn normalize_native_extra_known_marketplaces(
        root: &Utf16JsonProjection,
        file: &str,
    ) -> super::NativeExtraKnownMarketplacesResult {
        let semantics = test_semantics(root);
        normalize_native_extra_known_marketplaces_source(root, file, &semantics)
    }

    fn validate_native_extra_known_marketplaces_due(
        value: &Utf16JsonProjection,
    ) -> super::NativeExtraKnownMarketplacesDueResult {
        let semantics = test_semantics(value);
        validate_native_extra_known_marketplaces_due_source(value, &semantics)
    }

    #[test]
    fn invalid_root_is_removed_with_exact_warning_row() {
        let result = normalize_native_extra_known_marketplaces(
            &parse(r#"{"extraKnownMarketplaces":null}"#),
            "policyHelpers.defaultSettings",
        );
        assert!(!result.unresolved);
        assert!(result.normalized.is_none());
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].raw_warning.to_json_string().unwrap(),
            r#"{"file":"policyHelpers.defaultSettings","path":"extraKnownMarketplaces","message":"\"extraKnownMarketplaces\" must be an object mapping marketplace names to declarations; received null. This field was ignored.","severity":"warning","invalidValue":"null"}"#
        );
    }

    #[test]
    fn valid_settings_declaration_survives_while_name_mismatch_is_removed() {
        let result = normalize_native_extra_known_marketplaces(
            &parse(
                r#"{"extraKnownMarketplaces":{"acme-inline":{"source":{"source":"settings","name":"acme-inline","plugins":[]}},"wrong":{"source":{"source":"settings","name":"other","plugins":[]}}}}"#,
            ),
            "policyHelpers.defaultSettings",
        );
        assert!(!result.unresolved);
        assert_eq!(
            result.normalized.unwrap().to_json_string().unwrap(),
            r#"{"acme-inline":{"source":{"source":"settings","name":"acme-inline","plugins":[]}}}"#
        );
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(
            result.diagnostics[0].path.string_at(""),
            Some("extraKnownMarketplaces.wrong")
        );
        assert_eq!(
            result.diagnostics[0].message.string_at(""),
            Some("Invalid marketplace entry was ignored: key \"wrong\" must match the settings source name \"other\"")
        );
    }

    #[test]
    fn invalid_known_url_is_dropped_and_unknown_discriminator_is_left_for_due() {
        let invalid = normalize_native_extra_known_marketplaces(
            &parse(
                r#"{"extraKnownMarketplaces":{"bad":{"source":{"source":"url","url":"not a url"}}}}"#,
            ),
            "policyHelpers.defaultSettings",
        );
        assert!(!invalid.unresolved);
        assert_eq!(invalid.normalized.unwrap().to_json_string().unwrap(), "{}");
        assert_eq!(invalid.diagnostics.len(), 1);
        assert_eq!(
            invalid.diagnostics[0].message.string_at(""),
            Some("Invalid marketplace entry was ignored: source.url: Invalid URL")
        );

        let unknown = normalize_native_extra_known_marketplaces(
            &parse(
                r#"{"extraKnownMarketplaces":{"future":{"source":{"source":"future","value":true}}}}"#,
            ),
            "policyHelpers.defaultSettings",
        );
        assert!(!unknown.unresolved);
        assert!(unknown.diagnostics.is_empty());
        assert_eq!(
            unknown.normalized.unwrap().to_json_string().unwrap(),
            r#"{"future":{"source":{"source":"future","value":true}}}"#
        );
    }

    #[test]
    fn nested_plugin_union_preserves_dirty_refinements_and_collapses_aborted_branches() {
        let result = normalize_native_extra_known_marketplaces(
            &parse(
                r#"{"extraKnownMarketplaces":{"archive":{"source":{"source":"settings","name":"archive","plugins":[{"name":"plug","source":{"source":"archive","url":"not a url"}}]}},"archiveSha":{"source":{"source":"settings","name":"archiveSha","plugins":[{"name":"plug","source":{"source":"archive","url":"https://example.com/a.zip","sha256":"x"}}]}},"archiveType":{"source":{"source":"settings","name":"archiveType","plugins":[{"name":"plug","source":{"source":"archive","url":7}}]}},"npmRegistry":{"source":{"source":"settings","name":"npmRegistry","plugins":[{"name":"plug","source":{"source":"npm","package":"acme/pkg","registry":"ftp://npm.example.com/"}}]}},"npmBadUrl":{"source":{"source":"settings","name":"npmBadUrl","plugins":[{"name":"plug","source":{"source":"npm","package":"acme/pkg","registry":"not a url"}}]}},"npmPackage":{"source":{"source":"settings","name":"npmPackage","plugins":[{"name":"plug","source":{"source":"npm","package":"acme../pkg"}}]}}}}"#,
            ),
            "policyHelpers.defaultSettings",
        );
        assert!(!result.unresolved);
        assert_eq!(result.normalized.unwrap().to_json_string().unwrap(), "{}");
        let messages = result
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.string_at("").unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            vec![
                "Invalid marketplace entry was ignored: source.plugins.0.source.url: Invalid URL, source.plugins.0.source.url: Archive URLs must use https:// and must not point at a loopback, link-local, or cloud-metadata host",
                "Invalid marketplace entry was ignored: source.plugins.0.source.sha256: Must be a 64-character hex SHA-256 digest",
                "Invalid marketplace entry was ignored: source.plugins.0.source: Invalid input",
                "Invalid marketplace entry was ignored: source.plugins.0.source.registry: Registry must be an http(s) URL",
                "Invalid marketplace entry was ignored: source.plugins.0.source.registry: Registry must be an http(s) URL",
                "Invalid marketplace entry was ignored: source.plugins.0.source: Invalid input",
            ]
        );
    }

    #[test]
    fn due_strips_unknown_fields_at_each_go_object_and_keeps_record_order() {
        let result = validate_native_extra_known_marketplaces_due(&parse(
            r#"{"10":{"source":{"source":"github","repo":"acme/ten","unknownSource":true},"installLocation":"/cache/ten","autoUpdate":true,"unknownRoot":"drop"},"2":{"source":{"source":"settings","name":"2","plugins":[{"name":"plug","source":{"source":"github","repo":"acme/plugin","unknownPluginSource":1},"unknownPlugin":1,"description":"desc"}],"unknownSettings":2,"owner":{"name":"Acme","email":"a@example.test","unknownOwner":3}},"unknownRoot":"drop"},"alpha":{"source":{"source":"url","url":"https://example.test/m.json","headers":{"Authorization":"ok"},"unknownSource":4},"unknownRoot":5}}"#,
        ));
        assert!(!result.unresolved);
        assert!(result.issues.is_empty());
        assert_eq!(
            result.normalized.unwrap().to_json_string().unwrap(),
            r#"{"2":{"source":{"source":"settings","name":"2","plugins":[{"name":"plug","source":{"source":"github","repo":"acme/plugin"},"description":"desc"}],"owner":{"name":"Acme","email":"a@example.test"}}},"10":{"source":{"source":"github","repo":"acme/ten"},"installLocation":"/cache/ten","autoUpdate":true},"alpha":{"source":{"source":"url","url":"https://example.test/m.json","headers":{"Authorization":"ok"}}}}"#
        );
    }

    #[test]
    fn due_checks_settings_key_name_after_all_go_values_and_in_js_entry_order() {
        let result = validate_native_extra_known_marketplaces_due(&parse(
            r#"{"10":{"source":{"source":"settings","name":"wrong-ten","plugins":[]}},"2":{"source":{"source":"settings","name":"wrong-two","plugins":[]}},"key":{"source":{"source":"settings","name":"different","plugins":[]}}}"#,
        ));
        assert!(!result.unresolved);
        assert!(result.normalized.is_none());
        assert_eq!(result.issues.len(), 3);
        assert_eq!(
            result.issues[0].path.to_json_string().unwrap(),
            r#"["extraKnownMarketplaces","2","source","name"]"#
        );
        assert_eq!(
            result.issues[0].message.string_at(""),
            Some("Settings-sourced marketplace name must match its extraKnownMarketplaces key (got key \"2\" but source.name \"wrong-two\")")
        );
        assert_eq!(
            result.issues[1].path.to_json_string().unwrap(),
            r#"["extraKnownMarketplaces","10","source","name"]"#
        );

        let invalid_value_wins = validate_native_extra_known_marketplaces_due(&parse(
            r#"{"mismatch":{"source":{"source":"settings","name":"other","plugins":[]}},"invalid":{"source":{"source":"github","repo":7}}}"#,
        ));
        assert!(!invalid_value_wins.unresolved);
        assert!(invalid_value_wins.normalized.is_none());
        assert_eq!(invalid_value_wins.issues.len(), 1);
        assert_eq!(
            invalid_value_wins.issues[0].path.to_json_string().unwrap(),
            r#"["extraKnownMarketplaces","invalid","source","repo"]"#
        );
        assert_eq!(
            invalid_value_wins.issues[0].message.string_at(""),
            Some("Invalid input: expected string, received number")
        );
    }

    #[test]
    fn due_key_name_message_and_path_keep_lone_surrogate_code_units() {
        let result = validate_native_extra_known_marketplaces_due(&parse(
            r#"{"\ud800":{"source":{"source":"settings","name":"x","plugins":[]}}}"#,
        ));
        assert!(!result.unresolved);
        assert_eq!(result.issues.len(), 1);
        assert_eq!(
            result.issues[0].path.to_json_string().unwrap(),
            r#"["extraKnownMarketplaces","\ud800","source","name"]"#
        );
        assert!(result.issues[0]
            .message
            .to_json_string()
            .unwrap()
            .contains(r#"\ud800"#));
    }

    #[test]
    fn due_empty_record_is_a_present_empty_normalized_map() {
        let result = validate_native_extra_known_marketplaces_due(&parse("{}"));
        assert!(!result.unresolved);
        assert!(result.issues.is_empty());
        assert_eq!(result.normalized.unwrap().to_json_string().unwrap(), "{}");
    }
}
