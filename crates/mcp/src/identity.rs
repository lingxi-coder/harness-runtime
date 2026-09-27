//! LingXi MCP `clientInfo` identity constants. Rebranded from claude-code
//! (whose `clientInfo.name` was `claude-code`); LingXi presents its own
//! identity to MCP servers.

use serde::Serialize;

/// Wire `clientInfo.name` value LingXi sends in the MCP `initialize` request.
pub const CLIENT_NAME: &str = "lingxi";
/// Wire `clientInfo.title` value.
pub const CLIENT_TITLE: &str = "LingXi";
/// Wire `clientInfo.version` — sourced from this crate's `Cargo.toml`.
pub const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Wire `clientInfo.description` value (debranded from claude-code's).
pub const CLIENT_DESCRIPTION: &str = "An agentic coding tool";
/// Public-facing website URL emitted in `clientInfo.websiteUrl` (matches
/// claude-code). Confirmed against binary at offset 200376067.
pub const MCP_WEBSITE_URL: &str = "https://claude.com/claude-code";

/// `clientInfo` payload sent during MCP `initialize`.
///
/// Wire shape: `{"name": "...", "title": "...", "version": "...",
/// "description": "...", "websiteUrl": "..."}`.
/// The `websiteUrl` key is camelCase per claude-code; serde rename is
/// applied explicitly on that single field.
#[derive(Debug, Clone, Serialize)]
pub struct ClientInfo {
    /// Logical client name (= [`CLIENT_NAME`]).
    pub name: &'static str,
    /// Human-readable title (= [`CLIENT_TITLE`]).
    pub title: &'static str,
    /// Semver-compatible version string (= [`CLIENT_VERSION`]).
    pub version: &'static str,
    /// Free-form description of the client (= [`CLIENT_DESCRIPTION`]).
    /// Binary-confirmed at offset 84000384; both initialize blocks send it
    /// (TS `client.ts:985,3280`).
    pub description: &'static str,
    /// Public marketing/landing-page URL (= [`MCP_WEBSITE_URL`]).
    #[serde(rename = "websiteUrl")]
    pub website_url: &'static str,
}

impl Default for ClientInfo {
    fn default() -> Self {
        Self {
            name: CLIENT_NAME,
            title: CLIENT_TITLE,
            version: CLIENT_VERSION,
            description: CLIENT_DESCRIPTION,
            website_url: MCP_WEBSITE_URL,
        }
    }
}

/// `const` constructor — kept alongside `Default::default()` so call sites
/// that prefer a `const`-context value (e.g. `static`-bound contexts) can
/// use it without depending on the runtime `Default` impl.
pub const CLIENT_INFO: ClientInfo = ClientInfo {
    name: CLIENT_NAME,
    title: CLIENT_TITLE,
    version: CLIENT_VERSION,
    description: CLIENT_DESCRIPTION,
    website_url: MCP_WEBSITE_URL,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_info_serializes_to_claude_code_literal() {
        let info = ClientInfo::default();
        let json = serde_json::to_string(&info).expect("serialize");
        // Field order is not guaranteed by serde_json — assert by parsing back.
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["name"], "lingxi");
        assert_eq!(parsed["title"], "LingXi");
        assert_eq!(parsed["description"], "An agentic coding tool");
        assert_eq!(parsed["websiteUrl"], "https://claude.com/claude-code");
        // CARGO_PKG_VERSION must be present and look like a semver triple.
        let version = parsed["version"].as_str().expect("version");
        assert!(
            version.split('.').count() >= 3,
            "version should be semver-like, got {version:?}"
        );
    }

    #[test]
    fn client_info_wire_bytes_carry_camelcase_website_url() {
        // Lock the camelCase field name — claude-code emits `websiteUrl`
        // (NOT `website_url`) inside `clientInfo`.
        let bytes = serde_json::to_vec(&ClientInfo::default()).expect("serialize");
        let s = std::str::from_utf8(&bytes).expect("utf8");
        assert!(
            s.contains(r#""websiteUrl":"https://claude.com/claude-code""#),
            "wire bytes must contain literal websiteUrl, got: {s}",
        );
        assert!(
            !s.contains("website_url"),
            "no snake_case leak in wire bytes"
        );
    }

    #[test]
    fn client_name_constant_is_literal_claude_code() {
        // Lock the wire constant against accidental renames.
        assert_eq!(CLIENT_NAME, "lingxi");
        assert_eq!(CLIENT_TITLE, "LingXi");
        assert_eq!(CLIENT_DESCRIPTION, "An agentic coding tool");
        assert_eq!(MCP_WEBSITE_URL, "https://claude.com/claude-code");
    }
}
