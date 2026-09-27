// lingxi-code/crates/core/src/settings/env_parser.rs
//! Walk a process-env snapshot, picking up keys under the `LINGXI_*` prefix
//! (clean break: no `CLAUDE_*` fallback).
//!
//! Each `SCREAMING_SNAKE_CASE` key is mapped to the camelCase field name in
//! [`crate::settings::schema::SettingsJson`].
//!
//! Multi-valued fields (arrays) use `:` as element separator (PATH-style, spec §7).

use crate::settings::schema::SettingsJson;
use crate::settings::SettingsError;
use std::collections::BTreeMap;

/// Settings env-override prefix. Clean break: LingXi reads only `LINGXI_*`
/// settings overrides (no `CLAUDE_*` fallback).
pub const PREFIX_PRIORITY: &[&str] = &["LINGXI_"];

/// Mapping from `SCREAMING_SNAKE_CASE` suffix → camelCase field name.
///
/// Each entry is `(suffix, field, kind)` where `kind` drives value parsing.
const FIELD_MAP: &[(&str, &str, FieldKind)] = &[
    ("MODEL", "model", FieldKind::Scalar),
    ("PROVIDER_REGION", "providerRegion", FieldKind::Scalar),
    ("TELEMETRY_ENABLED", "telemetryEnabled", FieldKind::Bool),
    (
        "TRUSTED_DIRECTORIES",
        "trustedDirectories",
        FieldKind::Array,
    ),
    (
        "ADDITIONAL_DIRECTORIES",
        "additionalDirectories",
        FieldKind::Array,
    ),
    ("ENABLED_TOOLS", "enabledTools", FieldKind::Array),
    ("MAX_EFFORT_LEVEL", "maxEffortLevel", FieldKind::Scalar),
    (
        "ADDITIONAL_INCLUDES",
        "additionalIncludes",
        FieldKind::Array,
    ),
];

#[derive(Debug, Clone, Copy)]
enum FieldKind {
    Scalar,
    Bool,
    Array,
}

/// Parse a process-env snapshot into a partial [`SettingsJson`] + a list of
/// invalid `(var, value)` pairs for telemetry.
///
/// # Errors
///
/// This function never returns `Err` directly — all parse failures are
/// reported via the invalid-list return so the loader can keep going.
/// The `Result` shape is preserved for API symmetry with the other modules.
pub fn parse_env(
    env: &BTreeMap<String, String>,
) -> Result<(SettingsJson, Vec<(String, String)>), SettingsError> {
    let mut chosen: BTreeMap<&'static str, String> = BTreeMap::new();
    let mut invalid: Vec<(String, String)> = Vec::new();

    // Walk prefixes highest-priority first; first writer wins per field.
    for prefix in PREFIX_PRIORITY {
        for (suffix, field, _kind) in FIELD_MAP {
            let key = format!("{prefix}{suffix}");
            if let Some(value) = env.get(&key) {
                chosen.entry(*field).or_insert_with(|| value.clone());
            }
        }
    }

    // Also surface any env var with a recognized prefix that we didn't
    // know how to map — these are likely typos worth telemetering.
    for (k, v) in env {
        let is_known_field = FIELD_MAP
            .iter()
            .any(|(suffix, _, _)| PREFIX_PRIORITY.iter().any(|p| format!("{p}{suffix}") == *k));
        if PREFIX_PRIORITY.iter().any(|p| k.starts_with(*p)) && !is_known_field {
            invalid.push((k.clone(), v.clone()));
        }
    }

    // Build the SettingsJson from chosen values.
    let mut out = SettingsJson::default();
    for (field, raw) in &chosen {
        let kind = FIELD_MAP
            .iter()
            .find_map(|(_, f, k)| (f == field).then_some(*k))
            .expect("chosen field must be in FIELD_MAP");
        match (kind, *field) {
            (FieldKind::Scalar, "model") => out.model = Some(raw.clone()),
            (FieldKind::Scalar, "providerRegion") => {
                match serde_json::from_value(serde_json::Value::String(raw.clone())) {
                    Ok(region) => out.provider_region = Some(region),
                    Err(_) => invalid.push(("LINGXI_PROVIDER_REGION".into(), raw.clone())),
                }
            }
            (FieldKind::Bool, "telemetryEnabled") => match raw.as_str() {
                "true" => out.telemetry_enabled = Some(true),
                "false" => out.telemetry_enabled = Some(false),
                _ => {
                    // Re-derive the canonical env-var name from the highest-priority prefix that holds this value.
                    let var = PREFIX_PRIORITY
                        .iter()
                        .map(|p| format!("{p}TELEMETRY_ENABLED"))
                        .find(|k| env.get(k).map(String::as_str) == Some(raw.as_str()))
                        .unwrap_or_else(|| "LINGXI_TELEMETRY_ENABLED".to_string());
                    invalid.push((var, raw.clone()));
                }
            },
            (FieldKind::Array, "trustedDirectories") => {
                out.trusted_directories = Some(raw.split(':').map(str::to_string).collect());
            }
            (FieldKind::Array, "additionalDirectories") => {
                out.additional_directories = Some(raw.split(':').map(str::to_string).collect());
            }
            (FieldKind::Array, "enabledTools") => {
                out.enabled_tools = Some(raw.split(':').map(str::to_string).collect());
            }
            (FieldKind::Array, "additionalIncludes") => {
                out.additional_includes = Some(raw.split(':').map(str::to_string).collect());
            }
            _ => unreachable!("FIELD_MAP / match arm mismatch — every entry must be handled"),
        }
    }

    Ok((out, invalid))
}

#[cfg(test)]
mod region_tests {
    use super::*;
    #[test]
    fn region_env_uses_the_shared_enum_and_rejects_unknown_values() {
        for (value, valid) in [
            ("international", true),
            ("china_mainland", true),
            ("global", false),
        ] {
            let env = BTreeMap::from([("LINGXI_PROVIDER_REGION".into(), value.into())]);
            let (settings, invalid) = parse_env(&env).unwrap();
            assert_eq!(settings.provider_region.is_some(), valid);
            assert_eq!(invalid.is_empty(), valid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    #[test]
    fn lingxi_prefix_is_read() {
        let env = env(&[("LINGXI_MODEL", "claude-opus-4-7")]);
        let (parsed, _) = parse_env(&env).unwrap();
        assert_eq!(parsed.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn array_value_splits_on_colon() {
        let env = env(&[("LINGXI_TRUSTED_DIRECTORIES", "/a:/b:/c")]);
        let (parsed, _) = parse_env(&env).unwrap();
        assert_eq!(
            parsed.trusted_directories.as_deref(),
            Some(&["/a".to_string(), "/b".to_string(), "/c".to_string()][..])
        );
    }

    #[test]
    fn bool_value_parses_true_false_only() {
        let env_ok = env(&[("LINGXI_TELEMETRY_ENABLED", "true")]);
        let (parsed, invalid) = parse_env(&env_ok).unwrap();
        assert_eq!(parsed.telemetry_enabled, Some(true));
        assert!(invalid.is_empty());

        let env_bad = env(&[("LINGXI_TELEMETRY_ENABLED", "yes")]);
        let (_, invalid) = parse_env(&env_bad).unwrap();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].0, "LINGXI_TELEMETRY_ENABLED");
        assert_eq!(invalid[0].1, "yes");
    }

    #[test]
    fn unknown_env_var_with_recognized_prefix_is_returned_in_invalid_list() {
        let env = env(&[("LINGXI_UNKNOWN_FIELD", "x")]);
        let (_, invalid) = parse_env(&env).unwrap();
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].0, "LINGXI_UNKNOWN_FIELD");
    }

    #[test]
    fn unrelated_env_vars_are_ignored() {
        let env = env(&[("PATH", "/usr/bin"), ("HOME", "/Users/x")]);
        let (parsed, invalid) = parse_env(&env).unwrap();
        assert!(parsed.model.is_none());
        assert!(invalid.is_empty());
    }
}
