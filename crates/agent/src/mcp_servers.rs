//! Agent frontmatter `mcpServers` → scoped [`mcp::McpServerConfig`]s.
//!
//! Port of claude `PRn`/`agentMcpSpecsToScopedConfigs` (2.1.251 @160975900):
//! the per-entry conversion BOTH the main-thread agent merge (`FWt`) and the
//! per-SUBAGENT-spawn connect (`Agr`) run before folding an agent's inline
//! `mcpServers` into their respective connect lists. String (by-name),
//! multi-key records, reserved names and internal-only IDE transports are
//! skipped — each with claude's exact debug log line — and every surviving
//! RECORD config is stamped `scope:"agent"` ([`mcp::ConfigScope::Agent`]) and
//! flagged `is_newly_created: true` (claude `isNewlyCreated:true` — `Agr`
//! connects it and tears it down on subagent exit); a resolved BY-NAME entry
//! is flagged `is_newly_created: false` (claude `isNewlyCreated:false` — `Agr`
//! reuses whatever connection already exists for that name and never tears it
//! down on this spawn's behalf).

use crate::definition::{AgentDefinition, AgentMcpServerSpec, AgentSource};

/// One converted per-agent MCP server config, paired with claude `PRn`'s
/// `isNewlyCreated` flag.
#[derive(Debug, Clone)]
pub struct ScopedAgentMcpServer {
    /// The agent-scoped (`ConfigScope::Agent`) or as-configured (by-name)
    /// server config.
    pub config: mcp::McpServerConfig,
    /// `true` ⇒ built FRESH from an inline record spec: the caller must
    /// CONNECT it (claude `Agr`'s `connectToServer`) and TEAR IT DOWN when
    /// the subagent spawn exits. `false` ⇒ resolved BY NAME against an
    /// existing config the caller already knows about: the caller reuses
    /// whatever connection already exists (or connects it through the
    /// ordinary shared path) and must NEVER disconnect it on this spawn's
    /// behalf.
    pub is_newly_created: bool,
}

/// Convert `def`'s frontmatter `mcpServers` into agent-scoped
/// [`ScopedAgentMcpServer`]s (claude `PRn`, called once per entry by both
/// `FWt` and `Agr`).
///
/// `strict_plugin_only_mcp` mirrors `Y0("mcp")` — the managed
/// `strictPluginOnlyCustomization` lock on the MCP slot. When set, agents from
/// non-plugin-trusted sources (`wke`: plugin / policySettings / built-in /
/// bundled) contribute NO servers. LingXi's composition root never populates
/// the strict policy today (`StrictPluginOnlyPolicy::empty()`), so production
/// callers pass `false`; the parameter keeps the gate 1:1 and testable.
///
/// `strict_mcp_config` mirrors `rx()` — the `--strict-mcp-config` CLI flag.
/// It gates ONLY the by-name (`ByName`) branch: a string spec resolves from
/// disk config, which `--strict-mcp-config` explicitly excludes, so it is
/// skipped with claude's exact copy rather than resolved.
///
/// `existing_configs` is searched (by `config.name`) to resolve a `ByName`
/// entry (claude `Zx(r)` — "look up an existing configured server by name");
/// callers pass whatever server list they already have in scope (the
/// session's discovered/dynamic MCP configs for the main-thread-agent case,
/// or the session's live server list for a subagent spawn). A name absent
/// from it is logged + skipped, matching claude's `if(!_) return null` →
/// `"[Agent: X] MCP server not found: <name>"`.
///
/// Skip rules, in claude's order per entry:
/// - string entry (`ByName`) under `--strict-mcp-config` — `MCP server '<name>'
///   skipped: string specs resolve from disk config, which --strict-mcp-config
///   ignores`;
/// - string entry (`ByName`) not found in `existing_configs` — `MCP server not
///   found: <name>`;
/// - record with != 1 key — `Invalid MCP server spec: expected exactly one key`;
/// - reserved server name (`BIt`) — `Skipping reserved MCP server name …`;
/// - `type: sse-ide` / `ws-ide` — `Skipping internal-only MCP transport …`;
/// - anything else is built via [`mcp::build_server_from_json_entry`] (the
///   `.mcp.json` entry shape — `{...i, scope:"agent"}`); a body that fails
///   validation is logged + skipped by that builder.
#[must_use]
pub fn agent_mcp_specs_to_scoped_configs(
    def: &AgentDefinition,
    strict_plugin_only_mcp: bool,
    strict_mcp_config: bool,
    existing_configs: &[mcp::McpServerConfig],
) -> Vec<ScopedAgentMcpServer> {
    if def.mcp_servers.is_empty() {
        return Vec::new();
    }
    if strict_plugin_only_mcp && !plugin_trusted_source(def.source) {
        tracing::warn!(
            "[Agent: {}] Skipping frontmatter MCP servers: strictPluginOnlyCustomization locks MCP to plugin-only (agent source: {})",
            def.agent_type,
            source_label(def.source)
        );
        return Vec::new();
    }
    let mut out: Vec<ScopedAgentMcpServer> = Vec::new();
    let mut upsert = |cfg: mcp::McpServerConfig, is_newly_created: bool| match out
        .iter_mut()
        .find(|s| s.config.name == cfg.name)
    {
        Some(slot) => {
            *slot = ScopedAgentMcpServer {
                config: cfg,
                is_newly_created,
            }
        }
        None => out.push(ScopedAgentMcpServer {
            config: cfg,
            is_newly_created,
        }),
    };
    for spec in &def.mcp_servers {
        let record = match spec {
            // claude `typeof r === "string"` (`PRn`): a by-name entry
            // resolves from an EXISTING config rather than being built here.
            AgentMcpServerSpec::ByName(name) => {
                if strict_mcp_config {
                    tracing::warn!(
                        "[Agent: {}] MCP server '{}' skipped: string specs resolve from disk config, which --strict-mcp-config ignores",
                        def.agent_type,
                        name
                    );
                    continue;
                }
                match existing_configs.iter().find(|c| &c.name == name) {
                    Some(cfg) => {
                        upsert(cfg.clone(), false);
                        continue;
                    }
                    None => {
                        tracing::warn!(
                            "[Agent: {}] MCP server not found: {}",
                            def.agent_type,
                            name
                        );
                        continue;
                    }
                }
            }
            AgentMcpServerSpec::Record(map) => map,
        };
        if record.len() != 1 {
            tracing::warn!(
                "[Agent: {}] Invalid MCP server spec: expected exactly one key",
                def.agent_type
            );
            continue;
        }
        let (name, raw) = record
            .iter()
            .next()
            .expect("record.len() == 1 guarantees one entry");
        if mcp::normalization::is_reserved_mcp_server_name(name) {
            tracing::warn!(
                "[Agent: {}] Skipping reserved MCP server name '{}' in frontmatter",
                def.agent_type,
                name
            );
            continue;
        }
        if let Some(ty @ ("sse-ide" | "ws-ide")) = raw.get("type").and_then(|v| v.as_str()) {
            tracing::warn!(
                "[Agent: {}] Skipping internal-only MCP transport '{}' for '{}' in frontmatter",
                def.agent_type,
                ty,
                name
            );
            continue;
        }
        // `t[o] = {...i, scope: "agent"}` — the body shares the `.mcp.json`
        // entry shape; an invalid body is logged + dropped by the builder.
        // A plain object assignment, so a name declared twice in one list is
        // LAST-wins and keeps its original key position; the `Vec` stands in
        // for `t`'s insertion order.
        if let Some(cfg) = mcp::build_server_from_json_entry(name, raw, mcp::ConfigScope::Agent) {
            let mut cfg = cfg;
            cfg.metadata.agent_source = Some(match def.source {
                AgentSource::BuiltIn => mcp::McpAgentSource::BuiltIn,
                AgentSource::Settings(protocol::SettingsScope::User) => {
                    mcp::McpAgentSource::UserSettings
                }
                AgentSource::Settings(protocol::SettingsScope::Project) => {
                    mcp::McpAgentSource::ProjectSettings
                }
                AgentSource::Settings(protocol::SettingsScope::Local) => {
                    mcp::McpAgentSource::LocalSettings
                }
                AgentSource::Plugin => mcp::McpAgentSource::Plugin,
                AgentSource::Settings(protocol::SettingsScope::Managed) => {
                    mcp::McpAgentSource::PolicySettings
                }
                AgentSource::Flag => mcp::McpAgentSource::FlagSettings,
                AgentSource::AdditionalDirectory => mcp::McpAgentSource::AdditionalDirectory,
            });
            upsert(cfg, true);
        }
    }
    out
}

/// claude `wke`: sources exempt from the strict plugin-only MCP lock —
/// `new Set(["plugin","policySettings","built-in","builtin","bundled"])`.
pub(crate) fn plugin_trusted_source(source: AgentSource) -> bool {
    matches!(
        source,
        AgentSource::Plugin
            | AgentSource::Settings(protocol::SettingsScope::Managed)
            | AgentSource::BuiltIn
    )
}

/// Human label for the strict-lock debug line (claude interpolates the raw
/// source string).
///
/// Delegates rather than restating: this was a byte-identical copy of
/// [`crate::handle::agent_source_to_claude_str`], and two hand-kept copies of
/// one table are exactly how the `tengu_agent_tool_selected` telemetry field
/// and this log line would drift apart without either test going red.
fn source_label(source: AgentSource) -> &'static str {
    crate::handle::agent_source_to_claude_str(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def_with_specs(specs: Vec<AgentMcpServerSpec>, source: AgentSource) -> AgentDefinition {
        let mut def = crate::catalog::parse_agent_from_json(
            "tester",
            &serde_json::json!({"description": "d", "prompt": "p"}),
            source,
        )
        .expect("minimal JSON agent parses");
        def.mcp_servers = specs;
        def
    }

    fn record(name: &str, body: serde_json::Value) -> AgentMcpServerSpec {
        let mut map = serde_json::Map::new();
        map.insert(name.to_string(), body);
        AgentMcpServerSpec::Record(map)
    }

    /// Convenience wrapper over the production signature for tests that don't
    /// care about `--strict-mcp-config` / by-name resolution.
    fn convert(def: &AgentDefinition, strict_plugin_only_mcp: bool) -> Vec<ScopedAgentMcpServer> {
        agent_mcp_specs_to_scoped_configs(def, strict_plugin_only_mcp, false, &[])
    }

    fn existing_stdio(name: &str, command: &str) -> mcp::McpServerConfig {
        mcp::build_server_from_json_entry(
            name,
            &serde_json::json!({"command": command}),
            mcp::ConfigScope::Settings(protocol::SettingsScope::User),
        )
        .expect("well-formed stdio entry builds")
    }

    #[test]
    fn empty_specs_yield_no_configs() {
        let def = def_with_specs(
            vec![],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        assert!(convert(&def, false).is_empty());
    }

    #[test]
    fn by_name_entry_not_found_is_skipped() {
        // claude `Zx(r)` returns nothing for an unknown name → `PRn` returns
        // `null` → "[Agent: X] MCP server not found: <name>", not fatal to
        // siblings.
        let def = def_with_specs(
            vec![AgentMcpServerSpec::ByName("slack".into())],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        assert!(agent_mcp_specs_to_scoped_configs(&def, false, false, &[]).is_empty());
    }

    #[test]
    fn by_name_entry_resolves_existing_config_as_not_newly_created() {
        // claude `PRn`: `{name:r, config:_, isNewlyCreated:false}` — REUSE the
        // existing config, do not stamp `scope:"agent"` or flag it as
        // freshly connected (the caller must never tear it down on this
        // spawn's behalf).
        let existing = vec![existing_stdio("slack", "slack-mcp")];
        let def = def_with_specs(
            vec![AgentMcpServerSpec::ByName("slack".into())],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        let cfgs = agent_mcp_specs_to_scoped_configs(&def, false, false, &existing);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].config.name, "slack");
        assert!(
            !cfgs[0].is_newly_created,
            "resolved-by-name is NOT newly created"
        );
        assert_eq!(
            cfgs[0].config.scope,
            mcp::ConfigScope::Settings(protocol::SettingsScope::User),
            "the EXISTING config's scope is preserved verbatim, not overwritten to Agent"
        );
    }

    #[test]
    fn inline_agent_source_is_persisted_while_by_name_keeps_config_identity() {
        let inline = def_with_specs(
            vec![record("inline", serde_json::json!({"command": "mcp"}))],
            AgentSource::Plugin,
        );
        let converted = convert(&inline, false);
        assert_eq!(converted.len(), 1);
        assert_eq!(
            converted[0].config.metadata.agent_source,
            Some(mcp::McpAgentSource::Plugin)
        );

        let existing = existing_stdio("shared", "shared-mcp");
        let by_name = def_with_specs(
            vec![AgentMcpServerSpec::ByName("shared".into())],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        let converted = agent_mcp_specs_to_scoped_configs(&by_name, false, false, &[existing]);
        assert_eq!(converted.len(), 1);
        assert_eq!(converted[0].config.metadata.agent_source, None);
    }

    #[test]
    fn by_name_entry_is_skipped_under_strict_mcp_config_even_when_it_exists() {
        // claude `PRn`: `if(rx()) return {skipped:"strict", name:r}` — checked
        // BEFORE the by-name lookup, so an existing config is never consulted.
        let existing = vec![existing_stdio("slack", "slack-mcp")];
        let def = def_with_specs(
            vec![AgentMcpServerSpec::ByName("slack".into())],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        assert!(agent_mcp_specs_to_scoped_configs(&def, false, true, &existing).is_empty());
    }

    #[test]
    fn inline_stdio_server_gets_agent_scope() {
        let def = def_with_specs(
            vec![record(
                "docs",
                serde_json::json!({"command": "npx", "args": ["-y", "docs-mcp"]}),
            )],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        let cfgs = convert(&def, false);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].config.name, "docs");
        assert_eq!(cfgs[0].config.scope, mcp::ConfigScope::Agent);
        assert!(
            cfgs[0].is_newly_created,
            "an inline record IS newly created"
        );
        assert!(matches!(
            &cfgs[0].config.spec,
            platform_api::McpTransportSpec::Stdio { command, .. } if command == "npx"
        ));
    }

    #[test]
    fn inline_http_server_parses_url_transport() {
        let def = def_with_specs(
            vec![record(
                "remote",
                serde_json::json!({"type": "http", "url": "https://mcp.example/api"}),
            )],
            AgentSource::Settings(protocol::SettingsScope::User),
        );
        let cfgs = convert(&def, false);
        assert_eq!(cfgs.len(), 1);
        assert!(matches!(
            &cfgs[0].config.spec,
            platform_api::McpTransportSpec::Http { url, .. } if url == "https://mcp.example/api"
        ));
    }

    #[test]
    fn multi_key_record_is_skipped_entirely() {
        // claude obs: `Object.entries(r).length !== 1` → warn + skip; NEITHER
        // server materializes.
        let mut map = serde_json::Map::new();
        map.insert("a".into(), serde_json::json!({"command": "x"}));
        map.insert("b".into(), serde_json::json!({"command": "y"}));
        let def = def_with_specs(
            vec![AgentMcpServerSpec::Record(map)],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        assert!(convert(&def, false).is_empty());
    }

    #[test]
    fn reserved_names_are_skipped() {
        for reserved in ["computer-use", "workspace", "claude-in-chrome"] {
            let def = def_with_specs(
                vec![record(reserved, serde_json::json!({"command": "x"}))],
                AgentSource::Settings(protocol::SettingsScope::Project),
            );
            assert!(
                convert(&def, false).is_empty(),
                "reserved name {reserved} must be skipped"
            );
        }
    }

    #[test]
    fn internal_only_ide_transports_are_skipped() {
        for ty in ["sse-ide", "ws-ide"] {
            let def = def_with_specs(
                vec![record(
                    "ide",
                    serde_json::json!({"type": ty, "url": "http://127.0.0.1:1"}),
                )],
                AgentSource::Settings(protocol::SettingsScope::Project),
            );
            assert!(
                convert(&def, false).is_empty(),
                "transport {ty} must be skipped"
            );
        }
    }

    #[test]
    fn invalid_body_is_dropped_but_siblings_survive() {
        let def = def_with_specs(
            vec![
                record("broken", serde_json::json!({"nope": true})),
                record("ok", serde_json::json!({"command": "x"})),
            ],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        let cfgs = convert(&def, false);
        assert_eq!(cfgs.len(), 1);
        assert_eq!(cfgs[0].config.name, "ok");
    }

    #[test]
    fn duplicate_names_are_last_wins_and_keep_their_position() {
        // `obs` accumulates into an OBJECT (`t[o] = {...i, scope:"agent"}`), so
        // the second `docs` overwrites the first — and JS keeps the key at its
        // original insertion position, ahead of `other`.
        let def = def_with_specs(
            vec![
                record("docs", serde_json::json!({"command": "first"})),
                record("other", serde_json::json!({"command": "x"})),
                record("docs", serde_json::json!({"command": "second"})),
            ],
            AgentSource::Settings(protocol::SettingsScope::Project),
        );
        let cfgs = convert(&def, false);
        assert_eq!(cfgs.len(), 2, "one config per name");
        assert_eq!(cfgs[0].config.name, "docs");
        assert_eq!(cfgs[1].config.name, "other");
        assert!(
            matches!(&cfgs[0].config.spec, platform_api::McpTransportSpec::Stdio { command, .. } if command == "second"),
            "the LAST entry for a name wins"
        );
    }

    #[test]
    fn strict_plugin_only_blocks_untrusted_sources_only() {
        let specs = || vec![record("docs", serde_json::json!({"command": "x"}))];
        // Untrusted (user/project/flag) sources: locked out.
        for src in [
            AgentSource::Settings(protocol::SettingsScope::User),
            AgentSource::Settings(protocol::SettingsScope::Project),
            AgentSource::Flag,
        ] {
            let def = def_with_specs(specs(), src);
            assert!(
                convert(&def, true).is_empty(),
                "{src:?} must be locked under strictPluginOnlyCustomization"
            );
        }
        // wke-trusted sources pass the lock.
        for src in [
            AgentSource::Plugin,
            AgentSource::Settings(protocol::SettingsScope::Managed),
            AgentSource::BuiltIn,
        ] {
            let def = def_with_specs(specs(), src);
            assert_eq!(
                convert(&def, true).len(),
                1,
                "{src:?} is wke-exempt from the strict lock"
            );
        }
    }
}
