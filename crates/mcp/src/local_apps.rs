//! Private Desktop-owned MCP discovery. This never modifies a user's MCP settings.
use crate::{ConfigScope, McpServerConfig, McpServerMetadata};
use lingxi_core::host::McpTransportSpec;
use serde::Deserialize;
use std::{fs, io::Read, path::Path};

/// Reserved discovery name. An explicitly configured same-name server wins.
pub const SERVER_NAME: &str = "lingxi-local-apps";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Descriptor {
    version: u32,
    port: u16,
    token: String,
    pid: u32,
}

/// Read only the supplied host's private, bounded loopback descriptor.
/// Missing/invalid discovery is unavailable, never a reason to use another home.
#[must_use]
pub fn discover(config_home: &Path) -> Option<McpServerConfig> {
    let path = config_home.join("desktop-local-app-mcp.json");
    let before = fs::symlink_metadata(&path).ok()?;
    if !before.is_file() || before.file_type().is_symlink() || before.len() > 4096 {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.mode() & 0o077 != 0 || before.uid() != fs::metadata(config_home).ok()?.uid() {
            return None;
        }
    }
    let file = fs::File::open(&path).ok()?;
    let opened = file.metadata().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return None;
        }
    }
    if !opened.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(4097).read_to_end(&mut bytes).ok()?;
    if bytes.len() > 4096 {
        return None;
    }
    let descriptor: Descriptor = serde_json::from_slice(&bytes).ok()?;
    if descriptor.version != 1
        || descriptor.port == 0
        || descriptor.pid == 0
        || descriptor.token.len() != 64
        || !descriptor.token.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some(McpServerConfig {
        name: SERVER_NAME.into(),
        spec: McpTransportSpec::Http {
            url: format!("http://127.0.0.1:{}/mcp", descriptor.port),
            headers: [(
                "Authorization".into(),
                format!("Bearer {}", descriptor.token),
            )]
            .into_iter()
            .collect(),
            headers_helper: None,
            oauth: None,
        },
        scope: ConfigScope::Dynamic,
        disabled: false,
        timeout_ms: Some(120_000),
        discovery_cache: Some(false),
        always_load: false,
        tools: Vec::new(),
        tool_permissions: Default::default(),
        config_error: None,
        metadata: McpServerMetadata::default(),
    })
}

/// Add the host-owned endpoint without replacing explicit user configuration.
pub fn append(configs: &mut Vec<McpServerConfig>, config_home: &Path) {
    if configs.iter().any(|config| config.name == SERVER_NAME) {
        return;
    }
    if let Some(config) = discover(config_home) {
        configs.push(config);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn write(home: &Path, value: serde_json::Value) {
        let path = home.join("desktop-local-app-mcp.json");
        fs::write(&path, value.to_string()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    fn descriptor() -> serde_json::Value {
        serde_json::json!({"version":1,"port":12345,"token":"a".repeat(64),"pid":1})
    }
    #[test]
    fn private_descriptor_uses_loopback_and_never_overwrites_configuration() {
        let home = tempfile::tempdir().unwrap();
        assert!(discover(home.path()).is_none());
        write(home.path(), descriptor());
        let config = discover(home.path()).unwrap();
        assert_eq!(config.discovery_cache, Some(false));
        match &config.spec {
            McpTransportSpec::Http { url, headers, .. } => {
                assert_eq!(url, "http://127.0.0.1:12345/mcp");
                assert_eq!(
                    headers["Authorization"],
                    format!("Bearer {}", "a".repeat(64))
                );
            }
            _ => panic!("expected standard HTTP MCP"),
        }
        let mut configs = vec![config];
        configs[0].disabled = true;
        append(&mut configs, home.path());
        assert_eq!(configs.len(), 1);
        assert!(configs[0].disabled);
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({"version":1,"port":0,"token":"a".repeat(64),"pid":1}),
            serde_json::json!({"version":1,"port":12345,"token":"invalid","pid":1}),
            serde_json::json!({"version":1,"port":12345,"token":"a".repeat(64),"pid":1,"url":"https://example.com"}),
        ] {
            write(home.path(), invalid);
            assert!(discover(home.path()).is_none());
        }
    }
    #[test]
    #[cfg(unix)]
    fn shared_or_symlinked_descriptor_is_not_discovered() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let home = tempfile::tempdir().unwrap();
        write(home.path(), descriptor());
        let path = home.path().join("desktop-local-app-mcp.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(discover(home.path()).is_none());
        let other = home.path().join("other.json");
        fs::rename(&path, &other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(other, path).unwrap();
        assert!(discover(home.path()).is_none());
    }
}
