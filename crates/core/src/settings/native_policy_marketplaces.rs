//! Source-aware normalization for Native marketplace policy lists.
//!
//! Source binding: Claude Code 2.1.295 raw shipped chunk
//! `native-nl-source/src_187673145.js`, SHA-256
//! `6fbf3e921382e88e71bd834f2034ee24a15cada553c00e95181e44fa033d22aa`.
//! `Fi`/`ln`/`No` define the entry schemas (local offsets 157644-157832),
//! `g2n` and `Sn` apply the post-schema enforceability checks (145985 and
//! 278862), and `la` performs ordered per-entry filtering plus the blocklist
//! aggregate diagnostic (279559). `os` installs the tolerant field handlers
//! and their outer fallbacks (287594 onward).
//!
//! The projection API retains exact UTF-16 strings and marketplace array
//! order. Regex compilation is only treated as known for a conservative
//! ECMAScript subset; patterns outside it leave the whole field untouched and
//! set [`NativePolicyMarketplaceResult::unresolved`]. `fancy-regex` is used
//! only for that subset so Rust's basic `regex` dialect cannot reject native
//! lookarounds or backreferences. This is not a claim that `fancy-regex` is a
//! complete ECMAScript `RegExp` implementation. URL-bearing marketplace
//! schemas also remain unresolved because Rust URL admission has not been
//! proven equivalent to the shipped JavaScript `URL` constructor.

use super::native_policy_js_semantics::{
    NativePolicyJsSemanticsRegistry, NativePolicyZodUrlOutcome,
};
use super::native_policy_parser::{NativePolicyDiagnosticKind, NativePolicyParseDiagnostic};
use super::native_policy_regex::{
    NativePolicyRegexValidationOutcome, NativePolicyRegexValidationRow,
};
use crate::types::utf16_json::{JsonValueView, Utf16JsonProjection};

/// Result for one present `strictKnownMarketplaces` or `blockedMarketplaces`
/// value. `normalized: None` means Native reads the field as unset; a present
/// empty projection is a real empty array. If `unresolved` is true, the
/// projection is the original field value and must not be treated as fully
/// normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativePolicyMarketplaceResult {
    pub(crate) normalized: Option<Utf16JsonProjection>,
    pub(crate) diagnostics: Vec<NativePolicyParseDiagnostic>,
    pub(crate) unresolved: bool,
}

/// Collect only dynamic inputs reached while `At` parses the two Native
/// marketplace-policy lists. Unrelated object fields named `url`,
/// `hostPattern`, or `pathPattern` are deliberately ignored.
pub(crate) fn collect_native_policy_marketplace_semantic_inputs(
    source: &Utf16JsonProjection,
    regex_patterns: &mut Vec<Vec<u16>>,
    url_inputs: &mut Vec<Vec<u16>>,
) {
    for field in ["strictKnownMarketplaces", "blockedMarketplaces"] {
        let Ok(value) = source.subprojection(&format!("/{field}")) else {
            continue;
        };
        let Some(root) = value.at_pointer("") else {
            continue;
        };
        let Some(length) = root.array_len() else {
            continue;
        };
        for index in 0..length {
            let Ok(entry_projection) = value.subprojection(&format!("/{index}")) else {
                continue;
            };
            let Some(entry) = entry_projection.at_pointer("") else {
                continue;
            };
            if entry.object_entries().is_none() {
                continue;
            }
            let Some(kind) = entry.get_property("source").and_then(JsonValueView::as_str) else {
                continue;
            };
            match kind {
                "hostPattern" | "pathPattern" => {
                    let key = if kind == "hostPattern" {
                        "hostPattern"
                    } else {
                        "pathPattern"
                    };
                    if let Some(pattern) =
                        entry.get_property(key).and_then(JsonValueView::code_units)
                    {
                        super::native_policy::NativePolicySemanticInputs::push_unique(
                            regex_patterns,
                            pattern.to_vec(),
                        );
                    }
                }
                "url" => {
                    if required_string(entry, "url") && optional_string_record(entry, "headers") {
                        if let Some(url) = entry
                            .get_property("url")
                            .and_then(JsonValueView::code_units)
                        {
                            super::native_policy::NativePolicySemanticInputs::push_unique(
                                url_inputs,
                                url.to_vec(),
                            );
                        }
                    }
                }
                "npm" => {
                    if !required_string(entry, "package")
                        || !optional_string(entry, "version")
                        || !optional_string(entry, "registry")
                    {
                        continue;
                    }
                    let Some(package) = get_string(entry, "package") else {
                        continue;
                    };
                    if !is_policy_npm_package(&package) {
                        continue;
                    }
                    if let Some(registry) = entry
                        .get_property("registry")
                        .and_then(JsonValueView::code_units)
                    {
                        super::native_policy::NativePolicySemanticInputs::push_unique(
                            url_inputs,
                            registry.to_vec(),
                        );
                    }
                }
                _ => {}
            }
        }
    }
}

/// Normalize one present Native marketplace policy field.
///
/// `value` is the field value itself, not the document object. The caller
/// handles missing properties. Explicit null removes either field without a
/// diagnostic. A non-array root uses Native's field-specific catch behavior:
/// strict marketplace allowlists fail closed to `[]`, while blocked
/// marketplace lists are dropped.
pub(crate) fn normalize_policy_marketplace_field(
    field: &str,
    value: &Utf16JsonProjection,
    regex_rows: &[NativePolicyRegexValidationRow],
    semantics: Option<&NativePolicyJsSemanticsRegistry>,
) -> NativePolicyMarketplaceResult {
    let Some(strict) = (match field {
        "strictKnownMarketplaces" => Some(true),
        "blockedMarketplaces" => Some(false),
        _ => None,
    }) else {
        return NativePolicyMarketplaceResult {
            normalized: Some(value.clone()),
            diagnostics: Vec::new(),
            unresolved: true,
        };
    };

    let Some(root) = value.at_pointer("") else {
        return unresolved(value);
    };
    if root.is_null() {
        return NativePolicyMarketplaceResult {
            normalized: None,
            diagnostics: Vec::new(),
            unresolved: false,
        };
    }

    let Some(length) = root.array_len() else {
        let (normalized, kind) = if strict {
            (
                Some(empty_array()),
                NativePolicyDiagnosticKind::FallbackEmpty,
            )
        } else {
            (None, NativePolicyDiagnosticKind::Ignored)
        };
        return NativePolicyMarketplaceResult {
            normalized,
            diagnostics: vec![diagnostic(field.to_owned(), kind)],
            unresolved: false,
        };
    };

    let mut normalized_entries = Vec::with_capacity(length);
    let mut diagnostics = Vec::new();
    let mut unresolved = false;

    for index in 0..length {
        let entry = value
            .subprojection(&format!("/{index}"))
            .expect("array item exists in the source projection");
        match parse_entry(field, &entry, regex_rows, semantics) {
            EntryParse::Invalid => diagnostics.push(diagnostic(
                format!("{field}[{index}]"),
                NativePolicyDiagnosticKind::InvalidEntryIgnored,
            )),
            EntryParse::Unresolved => unresolved = true,
            EntryParse::Valid {
                normalized: parsed,
                source,
            } => match enforceability(&source, regex_rows) {
                Enforceability::Enforceable => normalized_entries.push(parsed),
                Enforceability::Unenforceable => {
                    if strict {
                        diagnostics.push(diagnostic(
                            format!("{field}[{index}]"),
                            NativePolicyDiagnosticKind::InvalidEntryIgnored,
                        ));
                    } else {
                        normalized_entries.push(parsed);
                        diagnostics.push(diagnostic(
                            format!("{field}[{index}]"),
                            NativePolicyDiagnosticKind::UnenforceableEntryKept,
                        ));
                    }
                }
                Enforceability::Unresolved => unresolved = true,
            },
        }
    }

    if unresolved {
        return NativePolicyMarketplaceResult {
            normalized: Some(value.clone()),
            diagnostics,
            unresolved: true,
        };
    }

    if !strict && length > 0 && normalized_entries.is_empty() {
        diagnostics.push(diagnostic(
            field.to_owned(),
            NativePolicyDiagnosticKind::NoEnforceableBlockedEntries,
        ));
    }

    NativePolicyMarketplaceResult {
        normalized: Some(
            Utf16JsonProjection::array(normalized_entries)
                .expect("validated marketplace entries form a JSON array"),
        ),
        diagnostics,
        unresolved: false,
    }
}

fn unresolved(value: &Utf16JsonProjection) -> NativePolicyMarketplaceResult {
    NativePolicyMarketplaceResult {
        normalized: Some(value.clone()),
        diagnostics: Vec::new(),
        unresolved: true,
    }
}

fn string_projection_units(units: &[u16]) -> Result<Utf16JsonProjection, ()> {
    Utf16JsonProjection::root_string(String::from_utf16_lossy(units), units.to_vec())
        .map_err(|_| ())
}

fn empty_array() -> Utf16JsonProjection {
    Utf16JsonProjection::array(Vec::new()).expect("an empty JSON array is valid")
}

fn diagnostic(path: String, kind: NativePolicyDiagnosticKind) -> NativePolicyParseDiagnostic {
    NativePolicyParseDiagnostic {
        path,
        kind,
        status_only: false,
        substituted: false,
        synthesized: false,
        only_substitutes: false,
    }
}

enum EntryParse {
    Invalid,
    Unresolved,
    Valid {
        normalized: Utf16JsonProjection,
        source: MarketplaceSource,
    },
}

enum MarketplaceSource {
    Other,
    Url,
    Npm {
        registry_value_units: Option<Vec<u16>>,
    },
    Github {
        repo: String,
        reference: Option<String>,
    },
    Git {
        url: String,
        reference: Option<String>,
    },
    Regex {
        source: RegexSource,
        pattern: Vec<u16>,
    },
}

#[derive(Clone, Copy)]
enum RegexSource {
    Host,
    Path,
}

fn parse_entry(
    field: &str,
    entry: &Utf16JsonProjection,
    regex_rows: &[NativePolicyRegexValidationRow],
    semantics: Option<&NativePolicyJsSemanticsRegistry>,
) -> EntryParse {
    let Some(root) = entry.at_pointer("") else {
        return EntryParse::Invalid;
    };
    if root.object_entries().is_none() {
        return EntryParse::Invalid;
    }
    let Some(source) = root.get_property("source").and_then(JsonValueView::as_str) else {
        return EntryParse::Invalid;
    };

    let parsed = match source {
        "url" => {
            if !required_string(root, "url") || !optional_string_record(root, "headers") {
                return EntryParse::Invalid;
            }
            let units = get_string_units(root, "url").expect("required URL string checked");
            let Some(semantics) = semantics else {
                return EntryParse::Unresolved;
            };
            let Some(row) = semantics.url(units) else {
                return EntryParse::Unresolved;
            };
            let NativePolicyZodUrlOutcome::Valid {
                zod_value_units, ..
            } = &row.zod_url
            else {
                return EntryParse::Invalid;
            };
            let shape = vec!["source", "url", "headers"];
            let Ok(mut normalized) = entry.pick_object_fields(&shape) else {
                return EntryParse::Unresolved;
            };
            let Ok(url) = string_projection_units(zod_value_units) else {
                return EntryParse::Unresolved;
            };
            if normalized.set_field("url", url).is_err() {
                return EntryParse::Unresolved;
            }
            return EntryParse::Valid {
                normalized,
                source: MarketplaceSource::Url,
            };
        }
        "github" => {
            if !required_string(root, "repo")
                || !optional_string(root, "ref")
                || !optional_string(root, "path")
                || !optional_string_array(root, "sparsePaths")
                || !optional_bool(root, "skipLfs")
            {
                return EntryParse::Invalid;
            }
            let repo = get_string(root, "repo").expect("required string was checked");
            let reference = get_string(root, "ref");
            (
                vec!["source", "repo", "ref", "path", "sparsePaths", "skipLfs"],
                MarketplaceSource::Github { repo, reference },
            )
        }
        "git" => {
            if !required_string(root, "url")
                || !optional_string(root, "ref")
                || !optional_string(root, "path")
                || !optional_string_array(root, "sparsePaths")
                || !optional_bool(root, "skipLfs")
            {
                return EntryParse::Invalid;
            }
            let url = get_string(root, "url").expect("required string was checked");
            let reference = get_string(root, "ref");
            (
                vec!["source", "url", "ref", "path", "sparsePaths", "skipLfs"],
                MarketplaceSource::Git { url, reference },
            )
        }
        "npm" => {
            if !required_string(root, "package")
                || !optional_string(root, "version")
                || !optional_string(root, "registry")
            {
                return EntryParse::Invalid;
            }
            let package = get_string(root, "package").expect("required string was checked");
            if !is_policy_npm_package(&package) {
                return EntryParse::Invalid;
            }
            let registry_value_units = if let Some(registry) = root.get_property("registry") {
                let Some(units) = registry.code_units() else {
                    return EntryParse::Invalid;
                };
                let Some(semantics) = semantics else {
                    return EntryParse::Unresolved;
                };
                let Some(row) = semantics.url(units) else {
                    return EntryParse::Unresolved;
                };
                let NativePolicyZodUrlOutcome::Valid {
                    zod_value_units,
                    rdt_zod_value_valid,
                    ..
                } = &row.zod_url
                else {
                    return EntryParse::Invalid;
                };
                if !rdt_zod_value_valid {
                    return EntryParse::Invalid;
                }
                Some(zod_value_units.clone())
            } else {
                None
            };
            (
                vec!["source", "package", "version", "registry"],
                MarketplaceSource::Npm {
                    registry_value_units,
                },
            )
        }
        "file" | "directory" => {
            if !required_string(root, "path") {
                return EntryParse::Invalid;
            }
            (vec!["source", "path"], MarketplaceSource::Other)
        }
        "skills-dir" => (vec!["source"], MarketplaceSource::Other),
        "hostPattern" | "pathPattern" => {
            let field_name = if source == "hostPattern" {
                "hostPattern"
            } else {
                "pathPattern"
            };
            if !required_string(root, field_name) {
                return EntryParse::Invalid;
            }
            let pattern = entry
                .string_units(&format!("/{field_name}"))
                .expect("required pattern string was checked");
            let source_kind = if source == "hostPattern" {
                RegexSource::Host
            } else {
                RegexSource::Path
            };
            let Some(row) = regex_rows
                .iter()
                .find(|row| row.pattern_units.as_slice() == pattern.as_slice())
            else {
                return EntryParse::Unresolved;
            };
            if matches!(
                row.outcome,
                NativePolicyRegexValidationOutcome::Invalid { .. }
            ) {
                return EntryParse::Invalid;
            }
            (
                if source == "hostPattern" {
                    vec!["source", "hostPattern"]
                } else {
                    vec!["source", "pathPattern"]
                },
                MarketplaceSource::Regex {
                    source: source_kind,
                    pattern,
                },
            )
        }
        "settings" => return parse_settings_entry(entry, root),
        "pluginDirectory" if field == "blockedMarketplaces" => {
            (vec!["source"], MarketplaceSource::Other)
        }
        _ => return EntryParse::Invalid,
    };

    let (shape, source) = parsed;
    let mut normalized = entry
        .pick_object_fields(&shape)
        .expect("marketplace source shape fields are valid object members");
    match &source {
        MarketplaceSource::Npm {
            registry_value_units: Some(units),
        } => {
            let Ok(value) = string_projection_units(units) else {
                return EntryParse::Unresolved;
            };
            if normalized.set_field("registry", value).is_err() {
                return EntryParse::Unresolved;
            }
        }
        MarketplaceSource::Url
        | MarketplaceSource::Npm {
            registry_value_units: None,
        } => {}
        _ => {}
    }
    EntryParse::Valid { normalized, source }
}

fn parse_settings_entry(entry: &Utf16JsonProjection, root: JsonValueView<'_>) -> EntryParse {
    if !required_string(root, "name") || !optional_object(root, "owner") {
        return EntryParse::Invalid;
    }
    let Some(plugins) = root.get_property("plugins") else {
        return EntryParse::Invalid;
    };
    let Some(plugin_count) = plugins.array_len() else {
        return EntryParse::Invalid;
    };
    let Some(name_units) = root
        .get_property("name")
        .and_then(JsonValueView::code_units)
    else {
        return EntryParse::Invalid;
    };
    if !valid_policy_marketplace_name(name_units) {
        return EntryParse::Invalid;
    }
    if let Some(owner) = root.get_property("owner") {
        if !required_nonempty_string(owner, "name")
            || !optional_string(owner, "email")
            || !optional_string(owner, "url")
        {
            return EntryParse::Invalid;
        }
    }
    for index in 0..plugin_count {
        let Some(plugin) = plugins.get_index(index) else {
            return EntryParse::Invalid;
        };
        if plugin.object_entries().is_none()
            || !required_string(plugin, "name")
            || !optional_string(plugin, "description")
            || !optional_string(plugin, "version")
            || !optional_bool(plugin, "strict")
            || !optional_string_record(plugin, "headers")
        {
            return EntryParse::Invalid;
        }
        let Some(plugin_source) = plugin.get_property("source") else {
            return EntryParse::Invalid;
        };
        if plugin_source.object_entries().is_none() {
            // The plugin schema rejects string and unsupported sources even
            // when their source schema would otherwise accept them.
            return EntryParse::Invalid;
        }
    }
    if plugin_count > 0 {
        // `wg()` delegates to the larger remote-plugin `Bi()` source union.
        // Preserve these entries rather than approximating its validators.
        return EntryParse::Unresolved;
    }

    let normalized = entry
        .pick_object_fields(&["source", "name", "plugins", "owner"])
        .expect("settings marketplace fields are valid object members");
    let mut normalized = normalized;
    if let Ok(owner) = entry.subprojection("/owner") {
        let owner = owner
            .pick_object_fields(&["name", "email", "url"])
            .expect("owner fields are valid object members");
        normalized
            .set_field("owner", owner)
            .expect("owner exists in settings marketplace entry");
    }
    EntryParse::Valid {
        normalized,
        source: MarketplaceSource::Other,
    }
}

fn enforceability(
    source: &MarketplaceSource,
    regex_rows: &[NativePolicyRegexValidationRow],
) -> Enforceability {
    match source {
        MarketplaceSource::Other | MarketplaceSource::Url | MarketplaceSource::Npm { .. } => {
            Enforceability::Enforceable
        }
        MarketplaceSource::Github { repo, reference } => {
            if repo.contains('*') && !is_owner_wildcard(repo) {
                return Enforceability::Unenforceable;
            }
            if reference
                .as_ref()
                .is_some_and(|reference| reference.contains('*'))
            {
                return Enforceability::Unenforceable;
            }
            Enforceability::Enforceable
        }
        MarketplaceSource::Git { url, reference } => {
            if reference
                .as_ref()
                .is_some_and(|reference| reference.contains('*'))
            {
                return Enforceability::Unenforceable;
            }
            if contains_git_wildcard_marker(url) {
                // Native `Hi()` has URL/host canonicalization branches. A
                // wildcard URL therefore stays unmodified until those
                // branches can be reproduced exactly.
                return Enforceability::Unresolved;
            }
            Enforceability::Enforceable
        }
        MarketplaceSource::Regex { source, pattern } => {
            let _source_name = match source {
                RegexSource::Host => "hostPattern",
                RegexSource::Path => "pathPattern",
            };
            let Some(row) = regex_rows
                .iter()
                .find(|row| row.pattern_units.as_slice() == pattern.as_slice())
            else {
                return Enforceability::Unresolved;
            };
            match row.outcome {
                NativePolicyRegexValidationOutcome::Valid { .. } => Enforceability::Enforceable,
                NativePolicyRegexValidationOutcome::Invalid { .. } => Enforceability::Unenforceable,
            }
        }
    }
}

fn get_string_units<'a>(root: JsonValueView<'a>, key: &str) -> Option<&'a [u16]> {
    root.get_property(key).and_then(JsonValueView::code_units)
}

enum Enforceability {
    Enforceable,
    Unenforceable,
    Unresolved,
}

fn required_string(object: JsonValueView<'_>, key: &str) -> bool {
    object
        .get_property(key)
        .and_then(JsonValueView::as_str)
        .is_some()
}

fn required_nonempty_string(object: JsonValueView<'_>, key: &str) -> bool {
    object
        .get_property(key)
        .and_then(JsonValueView::code_units)
        .is_some_and(|units| !units.is_empty())
}

fn optional_string(object: JsonValueView<'_>, key: &str) -> bool {
    object
        .get_property(key)
        .is_none_or(|value| value.as_str().is_some())
}

fn optional_bool(object: JsonValueView<'_>, key: &str) -> bool {
    object
        .get_property(key)
        .is_none_or(|value| value.as_bool().is_some())
}

fn optional_string_array(object: JsonValueView<'_>, key: &str) -> bool {
    let Some(value) = object.get_property(key) else {
        return true;
    };
    let Some(length) = value.array_len() else {
        return false;
    };
    (0..length).all(|index| {
        value
            .get_index(index)
            .and_then(JsonValueView::as_str)
            .is_some()
    })
}

fn optional_string_record(object: JsonValueView<'_>, key: &str) -> bool {
    let Some(value) = object.get_property(key) else {
        return true;
    };
    let Some(entries) = value.object_entries() else {
        return false;
    };
    entries
        .into_iter()
        .all(|entry| entry.value.as_str().is_some())
}

fn optional_object(object: JsonValueView<'_>, key: &str) -> bool {
    object
        .get_property(key)
        .is_none_or(|value| value.object_entries().is_some())
}

fn get_string(object: JsonValueView<'_>, key: &str) -> Option<String> {
    object
        .get_property(key)
        .and_then(JsonValueView::as_str)
        .map(str::to_owned)
}

fn is_policy_npm_package(package: &str) -> bool {
    if package.contains("..") || package.contains("//") {
        return false;
    }
    if package.starts_with('@') {
        let Some((scope, name)) = package[1..].split_once('/') else {
            return false;
        };
        valid_npm_segment(scope) && (name == "*" || valid_npm_segment(name))
    } else {
        valid_npm_segment(package)
    }
}

fn valid_npm_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next().is_some_and(is_ascii_lower_alphanumeric)
        && chars.all(|character| {
            is_ascii_lower_alphanumeric(character) || matches!(character, '-' | '.' | '_')
        })
}

fn is_ascii_lower_alphanumeric(character: char) -> bool {
    character.is_ascii_lowercase() || character.is_ascii_digit()
}

fn is_owner_wildcard(repository: &str) -> bool {
    let Some(owner) = repository.strip_suffix("/*") else {
        return false;
    };
    if owner.is_empty() || owner.starts_with('-') || matches!(owner, "." | "..") {
        return false;
    }
    owner
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn contains_git_wildcard_marker(url: &str) -> bool {
    url.contains('*')
        || url
            .as_bytes()
            .windows(3)
            .any(|escape| escape.eq_ignore_ascii_case(b"%2a"))
}

fn valid_policy_marketplace_name(units: &[u16]) -> bool {
    if units.is_empty() {
        return false;
    }
    if units.contains(&u16::from(b' '))
        || units.contains(&u16::from(b'/'))
        || units.contains(&u16::from(b'\\'))
        || units
            .windows(2)
            .any(|pair| pair == [u16::from(b'.'), u16::from(b'.')])
        || units == [u16::from(b'.')]
    {
        return false;
    }
    let decoded = String::from_utf16_lossy(units);
    !decoded.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

#[cfg(test)]
enum RegexCompile {
    Compiles,
    DoesNotCompile,
    Unresolved,
}

#[cfg(test)]
fn compile_ecmascript_regex_subset(units: &[u16]) -> RegexCompile {
    let Ok(pattern) = String::from_utf16(units) else {
        return RegexCompile::Unresolved;
    };
    if definitely_unclosed_regex(&pattern) {
        return RegexCompile::DoesNotCompile;
    }
    if !is_supported_ecmascript_subset(&pattern) {
        return RegexCompile::Unresolved;
    }
    match fancy_regex::Regex::new(&pattern) {
        Ok(_) => RegexCompile::Compiles,
        // A parse failure here may reflect a Rust/ECMAScript dialect boundary;
        // retain the source entry and expose the field as unresolved.
        Err(_) => RegexCompile::Unresolved,
    }
}

#[cfg(test)]
fn definitely_unclosed_regex(pattern: &str) -> bool {
    let bytes = pattern.as_bytes();
    let mut in_class = false;
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                if index + 1 == bytes.len() {
                    return true;
                }
                index += 2;
            }
            b'[' if !in_class => {
                in_class = true;
                index += 1;
            }
            b']' if in_class => {
                in_class = false;
                index += 1;
            }
            b'(' if !in_class => {
                depth += 1;
                index += 1;
            }
            b')' if !in_class => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    in_class || depth > 0
}

/// Fence fancy-regex to ASCII ECMAScript constructs whose compile grammar is
/// understood here. This admits lookarounds and prior-group numeric
/// backreferences, but leaves named captures, Annex B escapes, Unicode
/// patterns, and PCRE-only extensions unresolved.
#[cfg(test)]
fn is_supported_ecmascript_subset(pattern: &str) -> bool {
    if !pattern.is_ascii() || pattern.bytes().any(|byte| byte == b'\n' || byte == b'\r') {
        return false;
    }
    let bytes = pattern.as_bytes();
    let mut index = 0usize;
    let mut in_class = false;
    let mut depth = 0usize;
    let mut captures_seen = 0usize;

    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\\' {
            let Some(escaped) = bytes.get(index + 1).copied() else {
                return false;
            };
            if in_class {
                match escaped {
                    b'd' | b'D' | b's' | b'S' | b'w' | b'W' | b'b' | b'f' | b'n' | b'r' | b't'
                    | b'v' => index += 2,
                    b'0' if !bytes.get(index + 2).is_some_and(u8::is_ascii_digit) => index += 2,
                    b'x' if has_hex_digits(bytes, index + 2, 2) => index += 4,
                    b'u' if has_hex_digits(bytes, index + 2, 4) => index += 6,
                    b'\\' | b']' | b'[' | b'^' | b'-' => index += 2,
                    _ => return false,
                }
                continue;
            }
            match escaped {
                b'd' | b'D' | b's' | b'S' | b'w' | b'W' | b'b' | b'B' | b'f' | b'n' | b'r'
                | b't' | b'v' => index += 2,
                b'0' if !bytes.get(index + 2).is_some_and(u8::is_ascii_digit) => index += 2,
                b'x' if has_hex_digits(bytes, index + 2, 2) => index += 4,
                b'u' if has_hex_digits(bytes, index + 2, 4) => index += 6,
                b'1'..=b'9' => {
                    if bytes.get(index + 2).is_some_and(u8::is_ascii_digit)
                        || usize::from(escaped - b'0') > captures_seen
                    {
                        return false;
                    }
                    index += 2;
                }
                b'\\' | b'.' | b'^' | b'$' | b'|' | b'?' | b'*' | b'+' | b'(' | b')' | b'['
                | b']' | b'{' | b'}' | b'/' | b'-' => index += 2,
                _ => return false,
            }
            continue;
        }

        match byte {
            b'[' if !in_class => {
                if bytes.get(index + 1) == Some(&b']')
                    || (bytes.get(index + 1) == Some(&b'^') && bytes.get(index + 2) == Some(&b']'))
                {
                    return false;
                }
                in_class = true;
                index += 1;
            }
            b']' if in_class => {
                in_class = false;
                index += 1;
            }
            b']' => return false,
            b'(' if !in_class => {
                if bytes.get(index + 1) == Some(&b'?') {
                    let tail = &bytes[index..];
                    let prefix_length = if tail.starts_with(b"(?:")
                        || tail.starts_with(b"(?=")
                        || tail.starts_with(b"(?!")
                    {
                        3
                    } else if tail.starts_with(b"(?<=") || tail.starts_with(b"(?<!") {
                        4
                    } else {
                        return false;
                    };
                    depth += 1;
                    index += prefix_length;
                } else {
                    captures_seen += 1;
                    depth += 1;
                    index += 1;
                }
            }
            b')' if !in_class => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
                index += 1;
            }
            b'{' if !in_class => {
                let Some(end) = quantifier_end(bytes, index) else {
                    return false;
                };
                index = end + 1;
            }
            b'}' if !in_class => return false,
            _ => index += 1,
        }
    }

    !in_class && depth == 0
}

#[cfg(test)]
fn has_hex_digits(bytes: &[u8], start: usize, count: usize) -> bool {
    bytes
        .get(start..start.saturating_add(count))
        .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
}

#[cfg(test)]
fn quantifier_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    let lower_start = index;
    while bytes.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    if index == lower_start {
        return None;
    }
    if bytes.get(index) == Some(&b',') {
        index += 1;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
    }
    if bytes.get(index) == Some(&b'}') {
        Some(index)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn normalize_policy_marketplace_field(
        field: &str,
        value: &Utf16JsonProjection,
    ) -> NativePolicyMarketplaceResult {
        let semantics = NativePolicyJsSemanticsRegistry::from_rows(Vec::new(), Vec::new())
            .expect("empty test semantic inputs");
        super::normalize_policy_marketplace_field(field, value, &[], Some(&semantics))
    }

    fn projection(input: &str) -> Utf16JsonProjection {
        Utf16JsonProjection::parse(input).expect("test fixture is JSON")
    }

    fn result_value(result: NativePolicyMarketplaceResult) -> Option<String> {
        result
            .normalized
            .map(|projection| projection.to_json_string().expect("projection serializes"))
    }

    #[test]
    fn null_and_outer_type_fallbacks_match_native() {
        let null = projection("null");
        for field in ["strictKnownMarketplaces", "blockedMarketplaces"] {
            let result = normalize_policy_marketplace_field(field, &null);
            assert_eq!(result.normalized, None);
            assert!(result.diagnostics.is_empty());
            assert!(!result.unresolved);
        }

        let wrong_root = projection("false");
        let strict = normalize_policy_marketplace_field("strictKnownMarketplaces", &wrong_root);
        assert_eq!(result_value(strict.clone()).as_deref(), Some("[]"));
        assert_eq!(
            strict.diagnostics[0].kind,
            NativePolicyDiagnosticKind::FallbackEmpty
        );

        let blocked = normalize_policy_marketplace_field("blockedMarketplaces", &wrong_root);
        assert_eq!(blocked.normalized, None);
        assert_eq!(
            blocked.diagnostics[0].kind,
            NativePolicyDiagnosticKind::Ignored
        );
    }

    #[test]
    fn strips_unknown_entry_keys_and_preserves_array_order() {
        let value = projection(
            r#"[{"source":"github","repo":"owner/repo","extra":1},{"source":"file","path":"/tmp/marketplace","extra":2}]"#,
        );
        let result = normalize_policy_marketplace_field("strictKnownMarketplaces", &value);
        assert_eq!(
            result_value(result.clone()).as_deref(),
            Some(
                r#"[{"source":"github","repo":"owner/repo"},{"source":"file","path":"/tmp/marketplace"}]"#
            )
        );
        assert!(result.diagnostics.is_empty());
        assert!(!result.unresolved);
    }

    #[test]
    fn strict_drops_but_blocked_keeps_unenforceable_entries() {
        let value = projection(
            r#"[{"source":"github","repo":"owner/repo*"},{"source":"git","url":"https://example.com/repo","ref":"release*"}]"#,
        );
        let strict = normalize_policy_marketplace_field("strictKnownMarketplaces", &value);
        assert_eq!(result_value(strict.clone()).as_deref(), Some("[]"));
        assert_eq!(strict.diagnostics.len(), 2);
        assert!(strict
            .diagnostics
            .iter()
            .all(|item| item.kind == NativePolicyDiagnosticKind::InvalidEntryIgnored));

        let blocked = normalize_policy_marketplace_field("blockedMarketplaces", &value);
        let expected = value.to_json_string().unwrap();
        assert_eq!(
            result_value(blocked.clone()).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(blocked.diagnostics.len(), 2);
        assert!(blocked
            .diagnostics
            .iter()
            .all(|item| { item.kind == NativePolicyDiagnosticKind::UnenforceableEntryKept }));
    }

    #[test]
    fn invalid_members_precede_blocked_all_invalid_aggregate() {
        let value = projection(r#"[null, {"source":"file"}]"#);
        let result = normalize_policy_marketplace_field("blockedMarketplaces", &value);
        assert_eq!(result_value(result.clone()).as_deref(), Some("[]"));
        assert_eq!(result.diagnostics.len(), 3);
        assert_eq!(result.diagnostics[0].path, "blockedMarketplaces[0]");
        assert_eq!(result.diagnostics[1].path, "blockedMarketplaces[1]");
        assert_eq!(result.diagnostics[2].path, "blockedMarketplaces");
        assert_eq!(
            result.diagnostics[2].kind,
            NativePolicyDiagnosticKind::NoEnforceableBlockedEntries
        );
    }

    #[test]
    fn lookarounds_and_numeric_backreferences_are_not_rejected_by_basic_regex_dialect() {
        for pattern in [r"^(?=github\.com$)github\.com$", r"^(abc)\1$"] {
            let units = pattern.encode_utf16().collect::<Vec<_>>();
            assert!(matches!(
                compile_ecmascript_regex_subset(&units),
                RegexCompile::Compiles
            ));
        }
        let unresolved = r"(?<named>github\.com)".encode_utf16().collect::<Vec<_>>();
        assert!(matches!(
            compile_ecmascript_regex_subset(&unresolved),
            RegexCompile::Unresolved
        ));
    }

    #[test]
    fn encoded_git_wildcards_remain_unresolved_until_url_canonicalization_is_modeled() {
        let value = projection(r#"[{"source":"git","url":"https://github.com/acme/%2A"}]"#);
        let result = normalize_policy_marketplace_field("strictKnownMarketplaces", &value);
        assert!(result.unresolved);
        let expected = value.to_json_string().unwrap();
        assert_eq!(result_value(result).as_deref(), Some(expected.as_str()));
    }
}
