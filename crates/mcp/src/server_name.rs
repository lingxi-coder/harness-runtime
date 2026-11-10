//! Authored MCP server-name identity, independent of Rust registry handles.
use lingxi_core::types::utf16_json::Utf16JsonProjection;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashSet;

/// Exact JavaScript UTF-16 server name. `as_str` is a display projection;
/// identity and hook serialization always use the original code units.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct McpServerName {
    units: Vec<u16>,
    display: String,
    storage_identity: String,
}

impl McpServerName {
    /// Construct an authored name, including isolated surrogate code units.
    #[must_use]
    pub fn from_utf16(units: Vec<u16>) -> Self {
        let display = String::from_utf16_lossy(&units);
        let storage_identity = String::from_utf16(&units).unwrap_or_else(|_| {
            let mut key = String::from("utf16-");
            for unit in &units {
                use std::fmt::Write as _;
                let _ = write!(&mut key, "{unit:04x}");
            }
            key
        });
        Self {
            display,
            units,
            storage_identity,
        }
    }
    /// Display-safe text; never use this projection for identity comparisons.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.display
    }
    /// Exact authored code units, used for native string equality.
    #[must_use]
    pub fn utf16_units(&self) -> &[u16] {
        &self.units
    }
    /// Whether this name has an ordinary Rust scalar representation.
    #[must_use]
    pub fn is_scalar(&self) -> bool {
        String::from_utf16(&self.units).is_ok()
    }
    /// Stable name within the separately selected scalar/raw credential
    /// namespace. Raw units never depend on an allocated registry handle.
    pub fn storage_identity(&self) -> &str {
        &self.storage_identity
    }
    /// The native model-facing normalization. Replacing an isolated unit by
    /// one display replacement character preserves this ASCII unit mapper.
    #[must_use]
    pub fn normalized(&self) -> String {
        crate::normalization::normalize_name_for_mcp(&self.display)
    }
    /// Stable candidate for a private registry handle. Catalog allocation
    /// checks every scalar authored name before choosing the final handle.
    fn handle_candidate(&self) -> String {
        let mut text = String::from("__lingxi_mcp_utf16_");
        for unit in &self.units {
            use std::fmt::Write as _;
            let _ = write!(&mut text, "{unit:04x}");
        }
        text
    }
}
impl From<String> for McpServerName {
    fn from(display: String) -> Self {
        Self {
            units: display.encode_utf16().collect(),
            storage_identity: display.clone(),
            display,
        }
    }
}
impl From<&str> for McpServerName {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}
impl From<&String> for McpServerName {
    fn from(value: &String) -> Self {
        value.as_str().into()
    }
}
impl std::fmt::Display for McpServerName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.display)
    }
}
impl Serialize for McpServerName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let source = Utf16JsonProjection::root_string(self.display.clone(), self.units.clone())
            .map_err(serde::ser::Error::custom)?
            .to_json_string()
            .map_err(serde::ser::Error::custom)?;
        serde_json::value::RawValue::from_string(source)
            .map_err(serde::ser::Error::custom)?
            .serialize(serializer)
    }
}
impl<'de> Deserialize<'de> for McpServerName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        let value = Utf16JsonProjection::parse(raw.get()).map_err(serde::de::Error::custom)?;
        let units = value
            .string_units("")
            .ok_or_else(|| serde::de::Error::custom("MCP server name must be a string"))?;
        Ok(Self::from_utf16(units))
    }
}

/// Host metadata stores source units as integers so generic serde Value
/// consumers (config fingerprints, snapshots) never decode an invalid Rust
/// string. Public provenance continues to serialize as the original string.
pub(crate) mod optional_name_units_serde {
    use super::*;
    pub fn serialize<S: Serializer>(
        value: &Option<McpServerName>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        value
            .as_ref()
            .map(McpServerName::utf16_units)
            .serialize(serializer)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<McpServerName>, D::Error> {
        Option::<Vec<u16>>::deserialize(deserializer)
            .map(|value| value.map(McpServerName::from_utf16))
    }
}

/// Assign private handles for non-scalar authored names after merging a
/// catalog by exact UTF-16 identity. Every ordinary scalar name is reserved
/// first and remains unchanged, including names that resemble our handles.
pub fn finalize_mcp_server_names(configs: &mut [crate::McpServerConfig]) {
    finalize_mcp_server_names_against(configs, &[]);
}

/// Allocate an incremental batch without colliding with scalar names or
/// private handles already present in the live catalog. An exact identity
/// replacement retains its prior handle.
pub fn finalize_mcp_server_names_against(
    configs: &mut [crate::McpServerConfig],
    existing: &[crate::McpServerConfig],
) {
    let mut occupied: HashSet<String> = configs
        .iter()
        .filter_map(|config| {
            let name = config.authored_name();
            name.is_scalar().then(|| name.as_str().to_owned())
        })
        .collect();
    occupied.extend(existing.iter().map(|config| config.name.clone()));
    for config in configs {
        let name = config.authored_name();
        if let Some(previous) = existing
            .iter()
            .find(|previous| previous.name_matches_utf16(name.utf16_units()))
        {
            config.name.clone_from(&previous.name);
            config.metadata.authored_name = previous.metadata.authored_name.clone();
            continue;
        }
        let conflicts_with_live_handle = existing.iter().any(|previous| {
            previous.name == name.as_str() && !previous.name_matches_utf16(name.utf16_units())
        });
        if name.is_scalar() && !conflicts_with_live_handle {
            config.name = name.as_str().to_owned();
            continue;
        }
        config.metadata.authored_name = Some(name.clone());
        let base = name.handle_candidate();
        let mut handle = base.clone();
        let mut suffix = 0u64;
        while !occupied.insert(handle.clone()) {
            suffix += 1;
            handle = format!("{base}_{suffix}");
        }
        config.name = handle;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_scalar_handle_name_retains_identity_without_rekeying_live_server() {
        let mut existing = crate::parse_mcp_json_string(
            r#"{"mcpServers":{"\ud800":{"command":"first"}}}"#,
            crate::ConfigScope::Dynamic,
        )
        .unwrap();
        let old_handle = existing[0].name.clone();
        let raw =
            serde_json::json!({"mcpServers":{old_handle.clone():{"command":"second"}}}).to_string();
        let mut incoming = crate::parse_mcp_json_string(&raw, crate::ConfigScope::Dynamic).unwrap();
        finalize_mcp_server_names_against(&mut incoming, &existing);
        assert_ne!(incoming[0].name, old_handle);
        assert!(incoming[0].name_matches_utf16(&old_handle.encode_utf16().collect::<Vec<_>>()));
        assert_eq!(incoming[0].storage_name(), old_handle);
        assert_eq!(existing[0].name, old_handle);
        assert_eq!(
            serde_json::to_string(&crate::McpServerProvenance::from_config(&incoming[0])).unwrap(),
            format!(r#"{{"name":"{old_handle}","source":"dynamic"}}"#)
        );
        finalize_mcp_server_names_against(&mut existing, &incoming);
        assert_ne!(existing[0].name, incoming[0].name);
    }
    #[test]
    fn native_raw_name_fixture_reaches_provenance_and_fqn() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_raw_names_2_1_322.json"))
                .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let configs = crate::parse_mcp_json_string(
                case["rawConfig"].as_str().unwrap(),
                crate::ConfigScope::Dynamic,
            )
            .unwrap();
            assert_eq!(configs.len(), 1);
            let units = case["nameUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_u64().unwrap() as u16)
                .collect::<Vec<_>>();
            assert_eq!(configs[0].authored_name_units(), units);
            assert_eq!(
                configs[0].normalized_name_for_mcp(),
                case["normalized"].as_str().unwrap()
            );
            assert_eq!(
                serde_json::to_string(&crate::McpServerProvenance::from_config(&configs[0]))
                    .unwrap(),
                case["expectedProvenanceJson"].as_str().unwrap()
            );
        }
    }
    #[test]
    fn authored_name_roundtrips_without_replacement_collisions() {
        let high = McpServerName::from_utf16(vec![0xd800]);
        let replacement: McpServerName = "\u{fffd}".into();
        assert_eq!(high.as_str(), replacement.as_str());
        assert_ne!(high, replacement);
        let json = serde_json::to_string(&high).unwrap();
        assert_eq!(json, r#""\ud800""#);
        assert_eq!(serde_json::from_str::<McpServerName>(&json).unwrap(), high);
    }

    #[test]
    fn raw_config_keys_keep_scalar_names_and_handle_collisions_distinct() {
        let raw = r#"{"mcpServers":{"\ud800":{"command":"high"},"\udfff":{"command":"low"},"\ufffd":{"command":"replacement"},"__lingxi_mcp_utf16_d800":{"command":"literal"},"normal":{"command":"ordinary"}}}"#;
        let configs = crate::parse_mcp_json_string(raw, crate::ConfigScope::Dynamic).unwrap();
        assert_eq!(configs.len(), 5);
        let high = configs
            .iter()
            .find(|config| config.name_matches_utf16(&[0xd800]))
            .unwrap();
        assert_eq!(high.name, "__lingxi_mcp_utf16_d800_1");
        assert_eq!(high.normalized_name_for_mcp(), "_");
        let metadata = serde_json::to_value(high).unwrap();
        assert_eq!(
            metadata["metadata"]["authored_name"],
            serde_json::json!([0xd800])
        );
        let roundtrip: crate::McpServerConfig = serde_json::from_value(metadata).unwrap();
        assert_eq!(roundtrip.authored_name_units(), vec![0xd800]);
        assert!(configs
            .iter()
            .any(|config| config.name == "__lingxi_mcp_utf16_d800"
                && config.metadata.authored_name.is_none()));
        assert!(configs
            .iter()
            .any(|config| config.name == "\u{fffd}" && config.metadata.authored_name.is_none()));
        let normal = configs
            .iter()
            .find(|config| config.name == "normal")
            .unwrap();
        assert!(normal.metadata.is_empty());
        assert!(!serde_json::to_string(normal).unwrap().contains("metadata"));
        assert_eq!(
            serde_json::to_string(&crate::McpServerProvenance::from_config(high)).unwrap(),
            r#"{"name":"\ud800","source":"dynamic"}"#
        );
    }

    #[test]
    fn exact_scope_precedence_and_plugin_prefix_survive_catalog_loading() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global.json");
        let project = dir.path().join(".mcp.json");
        std::fs::write(&global,r#"{"unused":"\udfff","mcpServers":{"\ud800":{"command":"user"},"\ufffd":{"command":"replacement"}}}"#).unwrap();
        std::fs::write(
            &project,
            r#"{"mcpServers":{"\ud800":{"command":"project"}}}"#,
        )
        .unwrap();
        let configs = crate::load_mcp_json_with_precedence(&project, &global);
        assert_eq!(configs.len(), 2);
        let mut high = configs
            .into_iter()
            .find(|config| config.name_matches_utf16(&[0xd800]))
            .unwrap();
        assert!(
            matches!(&high.spec,lingxi_core::host::McpTransportSpec::Stdio{command,..}if command=="project")
        );
        high.prepend_authored_name("plugin:fixture:");
        high.metadata.provenance_source = Some("plugin".into());
        assert_eq!(
            serde_json::to_string(&crate::McpServerProvenance::from_config(&high)).unwrap(),
            r#"{"name":"plugin:fixture:\ud800","source":"plugin"}"#
        );
        assert_eq!(high.normalized_name_for_mcp(), "plugin_fixture__");
        let local = crate::parse_local_config_mcp_servers(
            r#"{"projects":{"cwd":{"mcpServers":{"\ud800":{"command":"local"}}}}}"#,
            "cwd",
            crate::ConfigScope::Dynamic,
        )
        .unwrap();
        assert_eq!(local.len(), 1);
        assert!(local[0].name_matches_utf16(&[0xd800]));
    }
}
