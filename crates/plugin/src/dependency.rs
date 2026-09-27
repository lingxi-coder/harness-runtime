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
}

impl PluginDependency {
    /// Return the resolved `name@marketplace` identity.
    #[must_use]
    pub fn resolved_id(&self, owner_marketplace: &str) -> String {
        let marketplace = self.marketplace.as_deref().unwrap_or(owner_marketplace);
        format!("{}@{marketplace}", self.name)
    }
}

/// Parse every dependency shape accepted by the public manifest schema:
/// string/object array entries and the legacy `{name: version}` map.
pub fn parse_dependencies(value: Option<&Value>) -> Result<Vec<PluginDependency>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        Value::Array(entries) => entries.iter().map(parse_entry).collect(),
        Value::Object(entries) => entries
            .iter()
            .map(|(identity, requirement)| {
                let (name, marketplace) = split_identity(identity)?;
                let version = match requirement {
                    Value::String(version) if !version.trim().is_empty() => {
                        Some(version.trim().to_string())
                    }
                    Value::Null => None,
                    _ => {
                        return Err(format!(
                            "dependency \"{identity}\" version must be a string"
                        ));
                    }
                };
                Ok(PluginDependency {
                    name,
                    version,
                    marketplace,
                })
            })
            .collect(),
        _ => Err("dependencies must be an array or object".to_string()),
    }
}

fn parse_entry(value: &Value) -> Result<PluginDependency, String> {
    match value {
        Value::String(identity) => {
            let (name, marketplace) = split_identity(identity)?;
            Ok(PluginDependency {
                name,
                version: None,
                marketplace,
            })
        }
        Value::Object(_) => {
            let dependency: PluginDependency = serde_json::from_value(value.clone())
                .map_err(|error| format!("invalid plugin dependency: {error}"))?;
            validate(dependency)
        }
        _ => Err("dependency entries must be strings or objects".to_string()),
    }
}

fn split_identity(identity: &str) -> Result<(String, Option<String>), String> {
    let identity = identity.trim();
    if identity.is_empty() {
        return Err("dependency name cannot be empty".to_string());
    }
    match identity.split_once('@') {
        Some((name, marketplace)) if !name.is_empty() && !marketplace.is_empty() => {
            Ok((name.to_string(), Some(marketplace.to_string())))
        }
        Some(_) => Err(format!("invalid dependency identity \"{identity}\"")),
        None => Ok((identity.to_string(), None)),
    }
}

fn validate(mut dependency: PluginDependency) -> Result<PluginDependency, String> {
    dependency.name = dependency.name.trim().to_string();
    if dependency.name.is_empty() {
        return Err("dependency name cannot be empty".to_string());
    }
    dependency.marketplace = dependency
        .marketplace
        .map(|marketplace| marketplace.trim().to_string())
        .filter(|marketplace| !marketplace.is_empty());
    dependency.version = dependency
        .version
        .map(|version| version.trim().to_string())
        .filter(|version| !version.is_empty());
    if let Some(requirement) = &dependency.version {
        semver::VersionReq::parse(requirement).map_err(|error| {
            format!(
                "invalid version requirement for dependency \"{}\": {error}",
                dependency.name
            )
        })?;
    }
    Ok(dependency)
}

/// Merge dependency declarations while preserving first-seen order. Repeated
/// identities accumulate semver requirements for the installer to intersect.
pub fn merge_dependency_requirements(
    sources: impl IntoIterator<Item = PluginDependency>,
) -> Vec<(PluginDependency, Vec<String>)> {
    let mut order = Vec::<PluginDependency>::new();
    let mut requirements = HashMap::<(String, Option<String>), Vec<String>>::new();
    for mut dependency in sources {
        let key = (dependency.name.clone(), dependency.marketplace.clone());
        if !requirements.contains_key(&key) {
            order.push(dependency.clone());
        }
        if let Some(requirement) = dependency.version.take() {
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
    fn parses_all_public_dependency_shapes() {
        let value = serde_json::json!([
            "alpha",
            "beta@community",
            {"name":"gamma","version":"^2","marketplace":"approved"}
        ]);
        let parsed = parse_dependencies(Some(&value)).unwrap();
        assert_eq!(parsed[0].resolved_id("root"), "alpha@root");
        assert_eq!(parsed[1].resolved_id("root"), "beta@community");
        assert_eq!(parsed[2].version.as_deref(), Some("^2"));
    }

    #[test]
    fn intersects_repeated_requirements_against_available_version() {
        let dependencies = vec![
            PluginDependency {
                name: "shared".into(),
                version: Some("^1".into()),
                marketplace: None,
            },
            PluginDependency {
                name: "shared".into(),
                version: Some(">=1.4".into()),
                marketplace: None,
            },
        ];
        let merged = merge_dependency_requirements(dependencies);
        assert_eq!(merged.len(), 1);
        assert!(version_satisfies_all("1.5.0", &merged[0].1).unwrap());
        assert!(!version_satisfies_all("1.2.0", &merged[0].1).unwrap());
    }
}
