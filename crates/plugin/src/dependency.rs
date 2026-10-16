//! Typed plugin dependency declarations shared by discovery and CLI install.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// One plugin dependency from a plugin manifest or marketplace entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDependency {
    /// Dependency plugin name without a marketplace suffix.
    pub name: String,
    /// Optional semver requirement such as `^1.2`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Optional marketplace override; bare dependencies inherit their owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace: Option<String>,
    /// Optional source SHA recorded by Native dependency metadata. The current
    /// dependency selector checks `version`; this field is not a checkout pin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
}

impl PluginDependency {
    /// Return the resolved `name@marketplace` identity.
    #[must_use]
    pub fn resolved_id(&self, owner_marketplace: &str) -> String {
        let marketplace = self.marketplace.as_deref().unwrap_or(owner_marketplace);
        format!("{}@{marketplace}", self.name)
    }
}

/// Parse Native's dependency constraint rows. Only object rows with a
/// non-empty `name` and a string `version` or `sha` carry a constraint. Other
/// rows are ignored, as in Native's raw-manifest constraint projection.
pub fn parse_dependencies(value: Option<&Value>) -> Result<Vec<PluginDependency>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Value::Array(entries) = value else {
        return Ok(Vec::new());
    };
    Ok(entries.iter().filter_map(parse_entry).collect())
}

fn parse_entry(value: &Value) -> Option<PluginDependency> {
    let Value::Object(fields) = value else {
        return None;
    };
    let name = fields.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let version = fields
        .get("version")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let sha = fields
        .get("sha")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    if version.is_none() && sha.is_none() {
        return None;
    }
    let marketplace = fields
        .get("marketplace")
        .and_then(Value::as_str)
        .filter(|marketplace| !marketplace.is_empty())
        .map(ToOwned::to_owned);

    Some(PluginDependency {
        name: name.to_owned(),
        version,
        marketplace,
        sha,
    })
}

/// Merge dependency declarations while preserving first-seen order. Repeated
/// identities accumulate version requirements; Native's constraint map keeps
/// the last row's optional SHA metadata for an identity.
pub fn merge_dependency_requirements(
    sources: impl IntoIterator<Item = PluginDependency>,
) -> Vec<(PluginDependency, Vec<String>)> {
    let mut order = Vec::<PluginDependency>::new();
    let mut positions = HashMap::<(String, Option<String>), usize>::new();
    let mut requirements = HashMap::<(String, Option<String>), Vec<String>>::new();
    for dependency in sources {
        let key = (dependency.name.clone(), dependency.marketplace.clone());
        if let Some(index) = positions.get(&key).copied() {
            order[index].sha = dependency.sha.clone();
        } else {
            positions.insert(key.clone(), order.len());
            order.push(dependency.clone());
        }
        if let Some(requirement) = dependency.version {
            requirements.entry(key).or_default().push(requirement);
        } else {
            requirements.entry(key).or_default();
        }
    }
    order
        .into_iter()
        .map(|dependency| {
            let key = (dependency.name.clone(), dependency.marketplace.clone());
            let constraints = requirements.remove(&key).unwrap_or_default();
            (dependency, constraints)
        })
        .collect()
}

/// Verify that a concrete plugin version satisfies every accumulated range.
pub fn version_satisfies_all(version: &str, requirements: &[String]) -> Result<bool, String> {
    if requirements.is_empty() {
        return Ok(true);
    }
    let version = semver::Version::parse(version)
        .map_err(|error| format!("plugin version \"{version}\" is not valid semver: {error}"))?;
    requirements.iter().try_fold(true, |matches, raw| {
        let raw = raw.trim();
        if raw.is_empty() {
            return Ok(matches);
        }
        let requirement = semver::VersionReq::parse(raw).map_err(|error| {
            format!("invalid dependency version requirement \"{raw}\": {error}")
        })?;
        Ok(matches && requirement.matches(&version))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_object_constraints_and_preserves_sha_only_rows() {
        let value = serde_json::json!([
            {"name":"alpha","version":"^1"},
            {"name":"beta","version":"2.0.0","sha":"deadbeef","marketplace":"community"},
            {"name":"gamma","sha":"deadbeef","marketplace":"approved"}
        ]);
        let parsed = parse_dependencies(Some(&value)).unwrap();
        assert_eq!(parsed[0].resolved_id("root"), "alpha@root");
        assert_eq!(parsed[1].resolved_id("root"), "beta@community");
        assert_eq!(parsed[1].version.as_deref(), Some("2.0.0"));
        assert_eq!(parsed[1].sha.as_deref(), Some("deadbeef"));
        assert_eq!(parsed[2].version, None);
        assert_eq!(parsed[2].sha.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn legacy_map_and_string_rows_do_not_create_dependencies() {
        assert!(
            parse_dependencies(Some(&serde_json::json!({"alpha": "^1"})))
                .unwrap()
                .is_empty()
        );
        assert!(
            parse_dependencies(Some(&serde_json::json!(["alpha", "beta@community"])))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn malformed_native_constraint_rows_are_skipped_without_losing_valid_rows() {
        let value = serde_json::json!([
            {"name":"", "version":"^1"},
            {"name":"missing-constraint"},
            {"name":"invalid-version-type", "version": 1},
            {"name":"valid-sha", "version": 1, "sha":"abc"}
        ]);
        let parsed = parse_dependencies(Some(&value)).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "valid-sha");
        assert_eq!(parsed[0].version, None);
        assert_eq!(parsed[0].sha.as_deref(), Some("abc"));
    }

    #[test]
    fn intersects_repeated_requirements_against_available_version() {
        let dependencies = vec![
            PluginDependency {
                name: "shared".into(),
                version: Some("^1".into()),
                marketplace: None,
                sha: None,
            },
            PluginDependency {
                name: "shared".into(),
                version: Some(">=1.4".into()),
                marketplace: None,
                sha: Some("deadbeef".into()),
            },
        ];
        let merged = merge_dependency_requirements(dependencies);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].0.sha.as_deref(), Some("deadbeef"));
        assert!(version_satisfies_all("1.5.0", &merged[0].1).unwrap());
        assert!(!version_satisfies_all("1.2.0", &merged[0].1).unwrap());
    }

    #[test]
    fn dependency_sha_is_metadata_not_a_version_or_checkout_override() {
        let value = serde_json::json!([{"name":"pinned","sha":"not-a-version-range"}]);
        let parsed = parse_dependencies(Some(&value)).unwrap();
        let merged = merge_dependency_requirements(parsed);
        assert_eq!(merged[0].0.sha.as_deref(), Some("not-a-version-range"));
        assert!(merged[0].1.is_empty());
        assert!(version_satisfies_all("1.2.3", &merged[0].1).unwrap());
    }

    #[test]
    fn empty_string_version_constraint_behaves_as_the_native_wildcard() {
        let value = serde_json::json!([{"name":"any-version", "version":""}]);
        let parsed = parse_dependencies(Some(&value)).unwrap();
        assert_eq!(parsed[0].version.as_deref(), Some(""));
        assert!(version_satisfies_all("1.2.3", &[String::new()]).unwrap());
    }
}
