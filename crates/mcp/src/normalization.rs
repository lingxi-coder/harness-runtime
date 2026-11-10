//! Pure MCP name-normalization — 1:1 port of claude-code
//! `src/services/mcp/normalization.ts` (a standalone no-dependency module).
//!
//! Server names become the `<server>` token in the `mcp__<server>__<tool>`
//! tool full-name, which the Anthropic API requires to match
//! `^[a-zA-Z0-9_-]{1,64}$`. A server named `my.server` or `My Server` would
//! otherwise produce an invalid tool name and break EVERY tool on that server.
//!
//! ## Wiring status
//!
//! Applied at two boundaries:
//!
//! - the [`crate::client::McpClient`] FQN boundary (build in `list_tools`,
//!   strip in `call_tool`) so the round-trip is self-consistent for
//!   invalid-char names;
//! - the [`crate::registry::McpRegistry::connect`] rewrite site, which stamps
//!   the normalized `<server>` token into each discovered tool's `full_name`
//!   (the posix transport emits an EMPTY token, deferring the rewrite here),
//!   and the [`crate::registry::McpRegistry::get_client`] /
//!   [`crate::registry::McpRegistry::get_config`] normalize-match
//!   (`normalize(stored_key) == arg`, mirroring claude-code's
//!   `normalizeNameForMCP(client.name) === serverName`), which keeps the raw
//!   `config.name` for `/mcp` display while resolving a model-supplied
//!   normalized token.
//!
//! Individual `mcp__server__tool` entries ARE advertised to the model: the
//! composition root builds a per-tool `tool_mcp::MCPTool` for every discovered
//! tool (`tool_mcp::build_registered_mcp_tools`) and registers them into the
//! wire `ToolRegistry` (`engine-desktop` `register_mcp_tools`), so
//! `ConversationOrchestrator::build_wire_tools` serializes each tool under its
//! normalized FQN. The model-facing FQN normalizes BOTH the server AND the tool
//! segment (matching claude-code `buildMcpToolName`, `client.ts:1768`); the raw
//! wire tool name is kept on [`lingxi_core::host::McpToolDto::tool_name`] and recovered for
//! dispatch by [`crate::registry::McpRegistry::resolve_wire_tool_name`].

/// Normalize a server name to the API pattern `^[a-zA-Z0-9_-]{1,64}$` by
/// replacing every invalid character (including `.` and spaces) with `_`.
///
/// For claude.ai servers (names starting with `"claude.ai "`), additionally
/// collapse runs of `_` to a single `_` and strip leading/trailing `_`, so the
/// normalized name can't interfere with the `__` delimiter in tool full-names.
///
/// Faithful to `normalizeNameForMCP`. NOTE: like claude-code, this does NOT
/// truncate to 64 chars — the `{1,64}` length bound is the API's, not enforced
/// here.
#[must_use]
pub fn normalize_name_for_mcp(name: &str) -> String {
    lingxi_core::types::normalize_name_for_mcp(name)
}

/// Native 2.1.322 `ln`: bounded control diagnostic text. Isolated UTF-16
/// units, invisible characters and non-space separators are deleted.
#[must_use]
pub fn sanitize_control_name_units(units: &[u16]) -> String {
    static INVISIBLE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"[\p{Cc}\p{Cf}\p{Zl}\p{Zp}\p{Default_Ignorable_Code_Point}\x{2800}]")
            .expect("native identifier character classes")
    });
    let scalar = char::decode_utf16(units.iter().copied().take(4096))
        .filter_map(Result::ok)
        .collect::<String>();
    let stripped = INVISIBLE.replace_all(&scalar, "");
    let mut used = 0;
    stripped
        .chars()
        .filter(|character| {
            !matches!(
                *character,
                '\u{00a0}' | '\u{1680}' | '\u{2000}'
                    ..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}'
            )
        })
        .take_while(|character| {
            used += character.len_utf16();
            used <= 1024
        })
        .collect()
}

/// Whether `name` is a reserved MCP server name — claude-code 2.1.206's `TEt`
/// predicate, `lDe(e) || zbt(e) || i6n(e) || e === GCn`:
/// - `Bc(e) === "claude-in-chrome"` (`gE`)
/// - `Bc(e) === "computer-use"`
/// - `Bc(e) ∈ {Bc("Claude Preview"), Bc("Claude Browser")}` (`W2h`)
/// - `e === "workspace"` (`GCn`, matched RAW — not normalized)
///
/// `Bc` is [`normalize_name_for_mcp`]; the Chrome-preview names are derived
/// through it exactly as the `W2h` set is built.
#[must_use]
pub fn is_reserved_mcp_server_name(name: &str) -> bool {
    let normalized = normalize_name_for_mcp(name);
    normalized == "claude-in-chrome"
        || normalized == "computer-use"
        || normalized == normalize_name_for_mcp("Claude Preview")
        || normalized == normalize_name_for_mcp("Claude Browser")
        || name == "workspace"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_322_control_diagnostics_match_shipped_utf16_sanitizer() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../tests/fixtures/mcp_sdk_control_names_2_1_322.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let units = case["inputUnits"]
                .as_array()
                .unwrap()
                .iter()
                .map(|unit| u16::try_from(unit.as_u64().unwrap()).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(
                sanitize_control_name_units(&units),
                case["sanitized"].as_str().unwrap(),
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn normalization_322_matches_native_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/mcp_2_1_322.json"))
                .expect("native MCP fixture");
        for case in fixture["normalizationCases"].as_array().expect("cases") {
            assert_eq!(
                normalize_name_for_mcp(case["input"].as_str().unwrap()),
                case["expected"].as_str().unwrap(),
            );
        }
    }

    #[test]
    fn reserved_names_match_normalizer_and_raw() {
        assert!(is_reserved_mcp_server_name("claude-in-chrome"));
        assert!(is_reserved_mcp_server_name("computer-use"));
        assert!(is_reserved_mcp_server_name("Claude Preview"));
        assert!(is_reserved_mcp_server_name("Claude Browser"));
        assert!(is_reserved_mcp_server_name("workspace"));
        assert!(!is_reserved_mcp_server_name("workspaces"));
        assert!(!is_reserved_mcp_server_name("filesystem"));
    }

    #[test]
    fn replaces_invalid_chars_with_underscore() {
        assert_eq!(normalize_name_for_mcp("my.server"), "my_server");
        assert_eq!(normalize_name_for_mcp("My Server"), "My_Server");
        assert_eq!(normalize_name_for_mcp("a/b:c@d"), "a_b_c_d");
    }

    #[test]
    fn keeps_valid_chars() {
        assert_eq!(normalize_name_for_mcp("filesystem"), "filesystem");
        assert_eq!(normalize_name_for_mcp("my-server_1"), "my-server_1");
        assert_eq!(normalize_name_for_mcp("MOCK"), "MOCK");
    }

    #[test]
    fn claudeai_prefix_collapses_and_trims() {
        // "claude.ai foo" → replace → "claude_ai_foo" (no collapse needed)
        assert_eq!(normalize_name_for_mcp("claude.ai foo"), "claude_ai_foo");
        // dots+spaces produce adjacent underscores that collapse, and a leading
        // invalid char would produce a leading underscore that is trimmed.
        assert_eq!(normalize_name_for_mcp("claude.ai .a..b "), "claude_ai_a_b");
    }

    #[test]
    fn non_claudeai_does_not_collapse() {
        // Without the claude.ai prefix, adjacent underscores are PRESERVED
        // (faithful: only claude.ai names collapse).
        assert_eq!(normalize_name_for_mcp("a..b"), "a__b");
        assert_eq!(normalize_name_for_mcp(".lead"), "_lead");
    }

    #[test]
    fn idempotent_on_already_valid() {
        let n = normalize_name_for_mcp("my.server");
        assert_eq!(normalize_name_for_mcp(&n), n);
    }
}
