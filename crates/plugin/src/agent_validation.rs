//! Plugin agent frontmatter validation (spec §19.1 / D2).
//!
//! A plugin-provided agent file must never be able to escalate privilege via
//! frontmatter. Privileges must come from the manifest at install/enable
//! time — never from a markdown file the plugin author can mint at will.
//!
//! Three top-level frontmatter keys are privilege-relevant: `permissionMode`,
//! `mcpServers`, and `hooks`. §19.1 requires all three be handled the SAME
//! way, and never reach the agent's runtime execution state:
//!
//! - **Normal validation** ([`scan_plugin_agent_privileged_fields`]) never
//!   fails because one of these fields is present — it only reports which
//!   ones were found, so the caller can WARN and then strip them from the
//!   parsed `AgentDefinition` before it reaches any registry (see
//!   `manager::PluginManager::load_plugin`). A malformed or
//!   over-privileged field must not remove an otherwise-valid agent from
//!   the registry, and must never take the whole plugin down with it.
//! - **Strict validation** ([`validate_plugin_agent_frontmatter`]) FAILS the
//!   load outright the moment any of the three is present.
//!
//! Both validators still fail closed — `Err(InvalidFrontmatter)` — when the
//! frontmatter cannot be parsed as a YAML mapping at all; that is orthogonal
//! to the three privileged fields.

use thiserror::Error;

/// A privilege-relevant top-level frontmatter key that must never reach an
/// agent's runtime execution state (§19.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivilegedField {
    /// `permissionMode` (or its legacy snake-case spelling `permission_mode`).
    PermissionMode,
    /// `mcpServers` (or its legacy snake-case spelling `mcp_servers`).
    McpServers,
    /// `hooks`.
    Hooks,
}

impl PrivilegedField {
    /// The frontmatter key name, for warning/error messages.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            PrivilegedField::PermissionMode => "permissionMode",
            PrivilegedField::McpServers => "mcpServers",
            PrivilegedField::Hooks => "hooks",
        }
    }
}

impl From<PrivilegedField> for AgentValidationError {
    fn from(field: PrivilegedField) -> Self {
        match field {
            PrivilegedField::PermissionMode => AgentValidationError::PermissionModeForbidden,
            PrivilegedField::McpServers => AgentValidationError::McpServersForbidden,
            PrivilegedField::Hooks => AgentValidationError::HooksForbidden,
        }
    }
}

/// Failure modes for [`validate_plugin_agent_frontmatter`] (strict) and
/// [`scan_plugin_agent_privileged_fields`] (normal — only the
/// `InvalidFrontmatter` variant can come from it).
#[derive(Debug, Clone, Error)]
pub enum AgentValidationError {
    /// Strict validation: frontmatter declares `permissionMode` (or its
    /// legacy snake-case form).
    #[error("agent frontmatter cannot set permissionMode in plugin context")]
    PermissionModeForbidden,
    /// Strict validation: frontmatter declares `mcpServers` (or its legacy
    /// snake-case form).
    #[error("agent frontmatter cannot declare mcpServers in plugin context")]
    McpServersForbidden,
    /// Strict validation: frontmatter declares `hooks`.
    #[error("agent frontmatter cannot declare hooks in plugin context")]
    HooksForbidden,
    /// Frontmatter is not valid YAML, or is not a top-level mapping.
    #[error("invalid agent frontmatter: {0}")]
    InvalidFrontmatter(String),
}

/// Scan a plugin agent's frontmatter YAML for privilege-relevant top-level
/// fields (§19.1 "normal validation").
///
/// Parsing the mapping is security-sensitive: the agent catalog accepts the
/// camel-case `permissionMode` / `mcpServers` keys (plus legacy snake-case
/// spellings), and a textual scan could both miss valid YAML spellings and
/// reject harmless comments or values. This never fails because a
/// privileged field is present — only the caller decides what to do with
/// the returned list (warn + strip, in `manager::load_plugin`).
pub fn scan_plugin_agent_privileged_fields(
    yaml: &str,
) -> Result<Vec<PrivilegedField>, AgentValidationError> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|error| AgentValidationError::InvalidFrontmatter(error.to_string()))?;
    let Some(mapping) = value.as_mapping() else {
        return Err(AgentValidationError::InvalidFrontmatter(
            "expected a top-level mapping".to_owned(),
        ));
    };

    let mut found = Vec::new();
    for key in mapping.keys().filter_map(serde_yaml::Value::as_str) {
        match key {
            "permissionMode" | "permission_mode" => found.push(PrivilegedField::PermissionMode),
            "mcpServers" | "mcp_servers" => found.push(PrivilegedField::McpServers),
            "hooks" => found.push(PrivilegedField::Hooks),
            _ => {}
        }
    }
    Ok(found)
}

/// Strict variant of the same scan (§19.1 "strict validation"): rejects the
/// agent load outright the moment any privilege-relevant field is present,
/// rather than warning and stripping it. Returns the first offending field's
/// error in scan order (`permissionMode`, `mcpServers`, `hooks`).
pub fn validate_plugin_agent_frontmatter(yaml: &str) -> Result<(), AgentValidationError> {
    match scan_plugin_agent_privileged_fields(yaml)?
        .into_iter()
        .next()
    {
        Some(field) => Err(field.into()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_permission_mode() {
        let yaml = "name: x\npermission_mode: bypassPermissions\n";
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::PermissionModeForbidden)
        ));
    }

    #[test]
    fn rejects_catalog_permission_mode_spelling() {
        let yaml = "name: x\npermissionMode: bypassPermissions\n";
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::PermissionModeForbidden)
        ));
    }

    #[test]
    fn rejects_hooks_with_yaml_spacing() {
        let yaml = "name: x\nhooks : {}\n";
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::HooksForbidden)
        ));
    }

    /// §19.1 unifies all three fields under one contract: strict validation
    /// must now reject `mcpServers` exactly like it rejects `permissionMode`
    /// / `hooks`. Before this task `mcpServers` was the odd one out —
    /// `validate_plugin_agent_frontmatter` let it through and the manager
    /// silently cleared it instead (see `mcp_servers_do_not_propagate` in
    /// `manager.rs` for the normal-validation half of this contract).
    #[test]
    fn strict_validation_rejects_mcp_servers_too() {
        let yaml = "name: x\ndescription: y\nmcpServers:\n  - docs:\n      command: docs-mcp\n";
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::McpServersForbidden)
        ));
    }

    #[test]
    fn clean_frontmatter_passes() {
        let yaml = "name: x\ndescription: y\ntools: ['Read']\n";
        assert!(validate_plugin_agent_frontmatter(yaml).is_ok());
        assert!(scan_plugin_agent_privileged_fields(yaml)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn privilege_names_in_comments_and_values_are_allowed() {
        let yaml =
            "name: x\n# permissionMode: bypassPermissions\ndescription: 'hooks: are documented'\n";
        assert!(validate_plugin_agent_frontmatter(yaml).is_ok());
        assert!(scan_plugin_agent_privileged_fields(yaml)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn nested_privilege_keys_do_not_trigger_top_level_gate() {
        let yaml = "name: x\nmetadata:\n  permissionMode: bypassPermissions\n  hooks:\n    preToolUse: []\n";
        assert!(validate_plugin_agent_frontmatter(yaml).is_ok());
        assert!(scan_plugin_agent_privileged_fields(yaml)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn malformed_frontmatter_fails_closed() {
        let yaml = "name: [unterminated\n";
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::InvalidFrontmatter(_))
        ));
        assert!(matches!(
            scan_plugin_agent_privileged_fields(yaml),
            Err(AgentValidationError::InvalidFrontmatter(_))
        ));
    }

    #[test]
    fn non_mapping_frontmatter_fails_closed() {
        let yaml = "- name\n- x\n";
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::InvalidFrontmatter(_))
        ));
        assert!(matches!(
            scan_plugin_agent_privileged_fields(yaml),
            Err(AgentValidationError::InvalidFrontmatter(_))
        ));
    }

    /// Every other fixture in this module spells the privileged key BARE, so
    /// a passing result there could mean the scan is keyed on the unquoted
    /// token rather than on the key itself. YAML lets an author write the
    /// exact same key quoted (`"permissionMode": ...`), and serde's
    /// `#[serde(rename = "permissionMode")]` in `agent::catalog::Frontmatter`
    /// honours the quoted spelling identically — so if the scan missed it,
    /// STRICT validation would wave through a live escalation spelling.
    #[test]
    fn quoted_privileged_keys_are_detected_like_bare_ones() {
        let yaml = "name: x\ndescription: y\n\"permissionMode\": bypassPermissions\n'hooks': {}\n\"mcpServers\":\n  - evil\n";
        assert_eq!(
            scan_plugin_agent_privileged_fields(yaml).unwrap(),
            vec![
                PrivilegedField::PermissionMode,
                PrivilegedField::Hooks,
                PrivilegedField::McpServers,
            ],
            "quoted frontmatter keys must be detected exactly like bare ones"
        );
        assert!(matches!(
            validate_plugin_agent_frontmatter(yaml),
            Err(AgentValidationError::PermissionModeForbidden)
        ));
    }

    /// The decisive contract test (§19.1): for EACH of the three privilege
    /// fields, normal (scan) validation must detect it without ever failing
    /// because it is present, while strict validation must fail for every
    /// one of them. Each case is independent — inverting either half (make
    /// scan fail, or make strict pass) must turn this red.
    #[test]
    fn normal_validation_warns_strict_validation_fails() {
        let cases: [(&str, PrivilegedField); 3] = [
            (
                "permissionMode: bypassPermissions",
                PrivilegedField::PermissionMode,
            ),
            ("mcpServers:\n  - evil", PrivilegedField::McpServers),
            ("hooks:\n  PreToolUse: []", PrivilegedField::Hooks),
        ];
        for (field_yaml, expected) in cases {
            let yaml = format!("name: x\ndescription: y\n{field_yaml}\n");

            // Normal validation: detects the field, never errors over it.
            let scanned = scan_plugin_agent_privileged_fields(&yaml).unwrap_or_else(|e| {
                panic!("normal validation must not fail for {field_yaml:?}: {e}")
            });
            assert_eq!(
                scanned,
                vec![expected],
                "normal validation must report (but not fail on) {field_yaml:?}"
            );

            // Strict validation: rejects the same input outright.
            let strict = validate_plugin_agent_frontmatter(&yaml);
            assert!(
                strict.is_err(),
                "strict validation must fail for {field_yaml:?}, got {strict:?}"
            );
        }
    }
}
