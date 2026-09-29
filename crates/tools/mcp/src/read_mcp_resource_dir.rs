//! `ReadMcpResourceDirTool` (alias `ReadMcpResourceDir`) — 2.1.238 `Yme`
//! (`cc-238.js @224767196`), name `DSe` (`cc-238.js @222722746`).
//!
//! The third member of the MCP-resource trio. Like its two siblings it is NOT
//! in `getTools`' base list — `iJ` drops all three by name
//! (`let r=new Set([K8.name,Z8.name,Yme.name,Ky])`, `cc-238.js @230759264`) and
//! MCP discovery pushes them into the MCP tool partition only when a connected
//! server declares `capabilities.resources`
//! (`if(![K8,Z8].some((c)=>o.some((u)=>il(u,c.name))))o.push(K8,Z8,Yme)`,
//! `cc-238.js @225994181`). See `build_registered_mcp_tools`.
//!
//! The tool object declares no `isEnabled`, so `es()`'s default
//! `isEnabled:()=>!0` applies — once pushed, it is always advertised.
//!
//! ## Ported scope
//!
//! The oracle's `call` has three legs:
//!
//! 1. resolve the server (`Ami`) — must exist, be connected, and declare
//!    `capabilities.resources`;
//! 2. `if(!eA())` → `{resources:[],error:"Directory listing is not enabled in this build."}`
//!    where `eA(){return it("tengu_mcp_skills",!1)}` (`cc-238.js @224764491`) —
//!    a **default-false** gate, so this is the branch every shipped default
//!    build takes;
//! 3. `if(!serverDeclaresDirectoryRead(i.capabilities))` →
//!    `Server "<name>" does not support directory listing.`, then the paginated
//!    `resources/directory/read` RPC.
//!
//! All three legs are now ported, plus leg 4 (the RPC itself) — see
//! [`server_declares_directory_read`] for the predicate
//! (`caps.extensions["io.modelcontextprotocol/skills"].directoryRead === true`,
//! `FSf`) and [`mcp::McpClient::read_mcp_directory`] for the paginated
//! `resources/directory/read` driver (`Cpv`, 20-page cap, cursor omitted on the
//! first page, InvalidParams tolerated only after page 1).
//!
//! Production capability flow is now explicit: POSIX `initialize` decodes the
//! oracle's `io.modelcontextprotocol/skills.directoryRead === true` extension
//! into [`lingxi_core::host::ServerCapabilitiesDto::directory_read`], discovery cache
//! persists that DTO bit, and this tool resolves cached/live registry state,
//! lazy-dials if needed, then re-verifies the CURRENT live `Connected` DTO
//! before issuing `resources/directory/read`.
//!
//! [`server_declares_directory_read`] remains as the raw oracle predicate for
//! direct-client semantics and byte-faithful tests; the production tool path
//! does not treat missing raw capabilities as support.
//!
//! **INERT IN A DEFAULT BUILD.** Leg 2's `eA()` gate is
//! `it("tengu_mcp_skills", !1)` — default FALSE — so every shipped default
//! session returns [`NOT_ENABLED_ERROR`] and never reaches legs 3/4. The
//! machinery exists so that flipping the flag matches upstream rather than
//! diverging; it moves zero wire bytes today.

use async_trait::async_trait;
use once_cell::sync::Lazy;
use permission::result::PermissionMetadata;
use permission::{PermissionDecisionReason, PermissionResult};
use serde_json::{json, Value};

use tool_api::context::ToolUseContext;
use tool_api::progress::ToolProgressSender;
use tool_api::tool_trait::{
    DescriptionOptions, PromptOptions, Tool, ToolCallResult, ToolError, ToolStaticContext,
};
use tool_api::BuiltinToolContext;

/// Model-facing name (2.1.238 `DSe`, `cc-238.js @222722746`).
pub const READ_MCP_RESOURCE_DIR_TOOL_NAME: &str = "ReadMcpResourceDirTool";

/// Legacy alias (`aliases:["ReadMcpResourceDir"]`). Already present in the
/// port's alias-normalisation tables (`permission/src/rule.rs`,
/// `llm-runtime/src/convert.rs`) — this is the implementation they pointed at.
pub const READ_MCP_RESOURCE_DIR_ALIAS: &str = "ReadMcpResourceDir";

/// The MCP-skills gate (`eA(){return it("tengu_mcp_skills",!1)}`). Default
/// FALSE, so the shipped binary's `call` returns [`NOT_ENABLED_ERROR`].
const MCP_SKILLS_GATE: &str = "tengu_mcp_skills";

/// Oracle `"inode/directory"` (`JBn`, `cc-238.js @222722720`).
pub const DIRECTORY_MIME_TYPE: &str = "inode/directory";

/// Oracle: `{data:{resources:[],error:"Directory listing is not enabled in this build."}}`.
const NOT_ENABLED_ERROR: &str = "Directory listing is not enabled in this build.";

/// Oracle `h_p` (`cc-238.js @222722775`). Note the LEADING newline — the
/// template literal opens with one — and the trailing newline before the
/// closing backtick.
const DESCRIPTION: &str = "
List the direct children of a directory resource on an MCP server.
- server: The name of the MCP server to read from
- uri: The URI of the directory resource

Only usable against a server that has declared support for directory listing. The listing is not recursive.
";

/// Oracle `g_p` (`cc-238.js @222723069`), with `${JBn}` interpolated.
const PROMPT: &str = "
List the direct children of a directory resource on an MCP server (`resources/directory/read`).

Parameters:
- server (required): The name of the MCP server to read from
- uri (required): The URI of the directory resource

The listing is not recursive. Each entry carries its own `uri`; subdirectories appear with mimeType \"inode/directory\" \u{2014} call this tool again on a subdirectory's `uri` to descend.

Only usable against a server that has declared support for directory listing; other servers return an error.
";

/// Oracle `$6S` (`cc-238.js @224766806`):
/// `be({server:H().describe("The MCP server name"),uri:H().describe("The directory resource URI to list")})`.
///
/// NOTE the key is `server`, not the `server_name` the port's two sibling
/// resource tools use — that divergence is pre-existing on those two and is not
/// replicated onto a tool being added fresh against the oracle.
static INPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "server": { "type": "string", "description": "The MCP server name" },
            "uri":    { "type": "string", "description": "The directory resource URI to list" }
        },
        "required": ["server", "uri"]
    })
});

/// Oracle `B6S` (`cc-238.js @224766934`).
static OUTPUT_SCHEMA: Lazy<Value> = Lazy::new(|| {
    json!({
        "type": "object",
        "properties": {
            "resources": {
                "type": "array",
                "description": "Direct children of the directory resource. Subdirectories appear with mimeType \"inode/directory\".",
                "items": {
                    "type": "object",
                    "properties": {
                        "uri":      { "type": "string", "description": "Child resource URI" },
                        "name":     { "type": "string", "description": "Child resource name" },
                        "mimeType": { "type": "string", "description": "Child MIME type" }
                    },
                    "required": ["uri", "name"]
                }
            },
            "error": {
                "type": "string",
                "description": "Human-readable error when the server could not list the directory"
            }
        },
        "required": ["resources"]
    })
});

/// `ReadMcpResourceDirTool` tool.
pub struct ReadMcpResourceDirTool {
    ctx: BuiltinToolContext,
}

impl ReadMcpResourceDirTool {
    /// Construct.
    #[must_use]
    pub fn new(ctx: BuiltinToolContext) -> Self {
        Self { ctx }
    }
}

fn is_shared_registry_entry(table_key: &str, state: &mcp::McpConnectionState) -> bool {
    table_key == state.config().name
}

fn ambiguous_server_message(requested: &str, candidates: &[String]) -> String {
    format!(
        "Server \"{requested}\" is ambiguous; matching servers: {}",
        candidates.join(", ")
    )
}

/// Oracle `Ami(clients, name)` (`cc-238.js @224079500`) — resolve a server by
/// name, erroring with the oracle's three messages in the oracle's order.
/// Returns the server's raw display name on success.
async fn resolve_server(registry: &mcp::McpRegistry, requested: &str) -> Result<String, ToolError> {
    use mcp::McpConnectionState;
    let conns = registry.connections.read().await;
    let mut names: Vec<String> = conns
        .iter()
        .filter(|&(table_key, state)| is_shared_registry_entry(table_key, state))
        .map(|(_table_key, state)| state.name().to_string())
        .collect();
    names.sort();

    if let Some(state) = conns.get(requested) {
        if is_shared_registry_entry(requested, state) {
            let name = state.name().to_string();
            let capabilities = match state {
                McpConnectionState::Connected { capabilities, .. }
                | McpConnectionState::Cached { capabilities, .. } => capabilities,
                _ => {
                    return Err(ToolError::InvalidInput(format!(
                        "Server \"{name}\" is not connected"
                    )));
                }
            };
            if !capabilities.resources {
                return Err(ToolError::InvalidInput(format!(
                    "Server \"{name}\" does not support resources"
                )));
            }
            return Ok(name);
        }
    }

    let requested_normalized = mcp::normalization::normalize_name_for_mcp(requested);
    let mut matched = conns
        .iter()
        .filter_map(|(table_key, state)| {
            if !is_shared_registry_entry(table_key, state) {
                return None;
            }
            let name = state.name();
            (mcp::normalization::normalize_name_for_mcp(name) == requested_normalized)
                .then_some(name.to_string())
        })
        .collect::<Vec<_>>();
    matched.sort();
    matched.dedup();
    let Some(name) = (match matched.len() {
        0 => None,
        1 => matched.pop(),
        _ => {
            return Err(ToolError::InvalidInput(ambiguous_server_message(
                requested, &matched,
            )));
        }
    }) else {
        return Err(ToolError::InvalidInput(format!(
            "Server \"{requested}\" not found. Available servers: {}",
            names.join(", ")
        )));
    };
    let state = conns
        .get(&name)
        .expect("shared raw name candidates must be present by exact key");
    let capabilities = match state {
        McpConnectionState::Connected { capabilities, .. }
        | McpConnectionState::Cached { capabilities, .. } => capabilities,
        _ => {
            return Err(ToolError::InvalidInput(format!(
                "Server \"{name}\" is not connected"
            )));
        }
    };
    if !capabilities.resources {
        return Err(ToolError::InvalidInput(format!(
            "Server \"{name}\" does not support resources"
        )));
    }
    Ok(name)
}

async fn live_resource_directory_capabilities(
    registry: &mcp::McpRegistry,
    requested_raw: &str,
) -> Result<(String, bool, bool), ToolError> {
    use mcp::McpConnectionState;
    let conns = registry.connections.read().await;
    let Some(state) = conns.get(requested_raw) else {
        return Err(ToolError::InvalidInput(format!(
            "Server \"{requested_raw}\" not found"
        )));
    };
    if !is_shared_registry_entry(requested_raw, state) {
        return Err(ToolError::InvalidInput(format!(
            "Server \"{requested_raw}\" not found"
        )));
    }
    match state {
        McpConnectionState::Connected {
            config,
            capabilities,
            ..
        } => Ok((
            config.name.clone(),
            capabilities.resources,
            capabilities.directory_read,
        )),
        _ => Err(ToolError::InvalidInput(format!(
            "Server \"{}\" is not connected",
            requested_raw
        ))),
    }
}

#[async_trait]
impl Tool for ReadMcpResourceDirTool {
    fn name(&self) -> &str {
        READ_MCP_RESOURCE_DIR_TOOL_NAME
    }

    fn aliases(&self) -> &[&str] {
        &[READ_MCP_RESOURCE_DIR_ALIAS]
    }

    /// Oracle `searchHint:"list the children of an MCP directory resource"`.
    fn search_hint(&self) -> Option<&str> {
        Some("list the children of an MCP directory resource")
    }

    fn user_facing_name(&self) -> Option<&str> {
        Some("readMcpResourceDir")
    }

    fn input_schema(&self) -> &Value {
        &INPUT_SCHEMA
    }

    fn output_schema(&self) -> Option<&Value> {
        Some(&OUTPUT_SCHEMA)
    }

    /// Oracle `maxResultSizeChars:1e5`.
    fn max_result_size_chars(&self) -> usize {
        100_000
    }

    /// Oracle `shouldDefer:!0`.
    fn should_defer(&self) -> bool {
        true
    }

    /// The object declares NO `isEnabled`, so `es()`'s default
    /// (`MFb = {isEnabled:()=>!0, …}`, `bin @284321674`) applies. Reachability
    /// is controlled entirely by whether MCP discovery pushed it into the tool
    /// partition, exactly as in the oracle.
    fn is_enabled(&self, _: &ToolStaticContext) -> bool {
        true
    }

    /// Oracle `isReadOnly(){return!0}`.
    fn is_read_only(&self, _: &Value) -> bool {
        true
    }

    /// Oracle `isConcurrencySafe(){return!0}`.
    fn is_concurrency_safe(&self, _: &Value) -> bool {
        true
    }

    async fn check_permissions(&self, _: &Value, _: &ToolUseContext) -> PermissionResult {
        PermissionResult::Allow {
            reason: PermissionDecisionReason::Other {
                reason: "list MCP directory resource".into(),
            },
            updated_input: None,
            update_destination: None,
            metadata: PermissionMetadata::default(),
        }
    }

    async fn description(&self, _: &Value, _: &DescriptionOptions) -> String {
        DESCRIPTION.into()
    }

    async fn prompt(&self, _: &PromptOptions) -> String {
        PROMPT.into()
    }

    async fn call(
        &self,
        input: Value,
        _ctx: ToolUseContext,
        _progress: ToolProgressSender,
    ) -> Result<ToolCallResult, ToolError> {
        let server = input
            .get("server")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidInput(
                    "ReadMcpResourceDirTool: missing or non-string server".into(),
                )
            })?
            .to_string();
        // `uri` is destructured alongside `server` in the oracle; validated here
        // so a malformed call fails before the gate branch.
        let uri = input
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolError::InvalidInput("ReadMcpResourceDirTool: missing or non-string uri".into())
            })?
            .to_string();

        let Some(registry) = self.ctx.mcp_registry.as_ref() else {
            return Err(ToolError::Io(
                "ReadMcpResourceDirTool: MCP registry not configured".into(),
            ));
        };

        // Leg 1 — oracle `let i=Ami(t,n)`, which THROWS before the gate check.
        let name = resolve_server(registry, &server).await?;

        // Leg 2 — oracle `if(!eA())return{data:{resources:[],error:…}}`.
        // `tengu_mcp_skills` is default-false, so this is the shipped default.
        if !telemetry::flag_bool(MCP_SKILLS_GATE, false) {
            return Ok(dir_error(NOT_ENABLED_ERROR.to_string()));
        }

        // Leg 3 — oracle
        // `if(!kJp().serverDeclaresDirectoryRead(i.capabilities))return{data:{resources:[],error:…}}`.
        let client = registry
            .ensure_connected_client(&name)
            .await
            .map_err(|e| ToolError::Io(e.to_string()))?;
        let (live_name, resources, directory_read) =
            live_resource_directory_capabilities(registry, &name).await?;
        if !resources {
            return Ok(dir_error(format!(
                "Server \"{live_name}\" does not support resources"
            )));
        }
        if !directory_read {
            return Ok(dir_error(format!(
                "Server \"{live_name}\" does not support directory listing."
            )));
        }

        // Leg 4 — oracle `a=await kJp().readMcpDirectory(s,o)`, wrapped in the
        // not-a-directory catch:
        //
        // ```js
        // catch(c){let u=N6S();
        //   if(u.isMcpNotADirectoryError(c))return $u(i.name,`resources/directory/read returned ${u.getMcpErrorCode(c)} \u2014 not a directory`),
        //     {data:{resources:[],error:`Not a directory resource: ${o}. If it is a file resource, use ${MSe} instead.`}};
        //   throw c}
        // ```
        //
        // `isMcpNotADirectoryError` is `e.code===Pc.InvalidParams` (-32602,
        // oracle @289723766) and `MSe` is `ReadMcpResourceTool`.
        let entries = match client.read_mcp_directory(&uri).await {
            Ok(entries) => entries,
            Err(e) => {
                let text = e.to_string();
                if text.contains(&format!("code={JSONRPC_INVALID_PARAMS}")) {
                    return Ok(dir_error(format!(
                        "Not a directory resource: {uri}. If it is a file resource, use \
                         {READ_MCP_RESOURCE_TOOL_NAME} instead."
                    )));
                }
                return Err(ToolError::Io(text));
            }
        };

        Ok(dir_listing(entries))
    }
}

/// JSON-RPC `InvalidParams`. `readMcpDirectory` surfaces the code inside its
/// error text (`remote error: code=-32602, message=…`), which is the only
/// channel the `McpClientError::Rpc(String)` seam preserves.
const JSONRPC_INVALID_PARAMS: i32 = -32602;

/// The sibling tool named in the not-a-directory message (oracle `MSe`).
const READ_MCP_RESOURCE_TOOL_NAME: &str = "ReadMcpResourceTool";

/// Oracle `FSf` / `jSf` (cc-238.js, both spellings identical):
///
/// ```js
/// var Eqn="io.modelcontextprotocol/skills";
/// function FSf(e){let t=e?.extensions?.[Eqn];
///   return t!=null&&typeof t==="object"&&"directoryRead"in t&&t.directoryRead===!0}
/// ```
///
/// Note the THREE conjuncts: the extension entry must be a non-null object, it
/// must CARRY the `directoryRead` key, and that key must be exactly `true` —
/// a truthy non-boolean does not qualify.
#[must_use]
pub fn server_declares_directory_read(raw_capabilities: Option<&Value>) -> bool {
    raw_capabilities
        .and_then(|c| c.get("extensions"))
        .and_then(|e| e.get(MCP_SKILLS_EXTENSION_KEY))
        .and_then(Value::as_object)
        .and_then(|ext| ext.get("directoryRead"))
        .is_some_and(|v| v.as_bool() == Some(true))
}

/// Oracle `Eqn` (cc-238.js @288269596) — the capability-extensions key the
/// MCP-skills directory-read declaration lives under.
pub const MCP_SKILLS_EXTENSION_KEY: &str = "io.modelcontextprotocol/skills";

/// Oracle `mapToolResultToToolResultBlockParam`'s SUCCESS branch:
///
/// ```js
/// let r=e.resources.map((o)=>`${o.name}${o.mimeType===JBn?"/":""}`).join(`\n`),
///     n=e.resources.length>0?`Directory listing (${e.resources.length} ${Et(e.resources.length,"entry","entries")}):\n${r}`:"Directory is empty.";
/// return{tool_use_id:t,type:"tool_result",content:`${n}\n\n${Ie(e)}`}
/// ```
///
/// `Ie(e)` is `JSON.stringify(e)` with no indent (the ERROR branch's
/// `Ie(e,null,2)` two-space form is only used by `isResultTruncated`), so the
/// model text is the human listing, a blank line, then the compact JSON of the
/// whole result object.
fn dir_listing(entries: Vec<mcp::McpDirectoryEntry>) -> ToolCallResult {
    let rendered = entries
        .iter()
        .map(|e| {
            let slash = if e.mime_type.as_deref() == Some(DIRECTORY_MIME_TYPE) {
                "/"
            } else {
                ""
            };
            format!("{}{slash}", e.name)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let header = if entries.is_empty() {
        "Directory is empty.".to_string()
    } else {
        format!(
            "Directory listing ({} {}):\n{rendered}",
            entries.len(),
            if entries.len() == 1 {
                "entry"
            } else {
                "entries"
            },
        )
    };
    let data = json!({ "resources": entries });
    let json_tail = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
    ToolCallResult {
        data,
        model_content: Some(format!("{header}\n\n{json_tail}")),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

/// Oracle `mapToolResultToToolResultBlockParam`: when `error` is set the model
/// sees the error text alone.
fn dir_error(error: String) -> ToolCallResult {
    ToolCallResult {
        data: json!({ "resources": [], "error": error }),
        model_content: Some(error),
        new_messages: vec![],
        context_modifier: None,
        is_error: false,
        mcp_meta: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_tool::cached_resource_test_support::{
        ctx_with_registry, new_cached_registry, resource_caps, seed_cached_server,
        CachedServerBehavior,
    };
    use once_cell::sync::Lazy;
    use std::sync::{Mutex, MutexGuard};

    static MCP_SKILLS_FLAG_TEST_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

    struct ScopedMcpSkillsFlag {
        _guard: MutexGuard<'static, ()>,
    }

    impl ScopedMcpSkillsFlag {
        fn set(value: bool) -> Self {
            let guard = MCP_SKILLS_FLAG_TEST_MUTEX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            telemetry::test_set_flag(MCP_SKILLS_GATE, value);
            Self { _guard: guard }
        }
    }

    impl Drop for ScopedMcpSkillsFlag {
        fn drop(&mut self) {
            telemetry::test_clear_flag(MCP_SKILLS_GATE);
        }
    }

    #[test]
    fn names_are_byte_exact() {
        assert_eq!(READ_MCP_RESOURCE_DIR_TOOL_NAME, "ReadMcpResourceDirTool");
        assert_eq!(READ_MCP_RESOURCE_DIR_ALIAS, "ReadMcpResourceDir");
        assert_eq!(DIRECTORY_MIME_TYPE, "inode/directory");
    }

    /// The alias tables in `permission/src/rule.rs` and `llm-runtime/src/convert.rs`
    /// already normalise `ReadMcpResourceDir` onto `ReadMcpResourceDirTool`;
    /// this is the implementation they were pointing at.
    #[test]
    fn alias_matches_the_permission_normalisation_table() {
        assert_eq!(
            permission::rule::normalize_legacy_tool_name(READ_MCP_RESOURCE_DIR_ALIAS),
            READ_MCP_RESOURCE_DIR_TOOL_NAME
        );
    }

    /// 2.1.238 `h_p` (`cc-238.js @222722775`) — a template literal that opens
    /// AND closes with a newline.
    #[test]
    fn description_matches_the_oracle_template() {
        assert_eq!(
            DESCRIPTION,
            "\nList the direct children of a directory resource on an MCP server.\n\
             - server: The name of the MCP server to read from\n\
             - uri: The URI of the directory resource\n\
             \n\
             Only usable against a server that has declared support for directory listing. The listing is not recursive.\n"
        );
    }

    /// 2.1.238 `g_p` (`cc-238.js @222723069`) with `${JBn}` interpolated, em
    /// dash included.
    #[test]
    fn prompt_interpolates_the_directory_mime_type_and_keeps_the_em_dash() {
        assert!(PROMPT.starts_with(
            "\nList the direct children of a directory resource on an MCP server (`resources/directory/read`).\n"
        ));
        assert!(PROMPT.contains(&format!(
            "subdirectories appear with mimeType \"{DIRECTORY_MIME_TYPE}\" \u{2014} call this tool again on a subdirectory's `uri` to descend."
        )));
        assert!(PROMPT.ends_with(
            "Only usable against a server that has declared support for directory listing; other servers return an error.\n"
        ));
    }

    /// The default-false `tengu_mcp_skills` gate's error text.
    #[test]
    fn not_enabled_error_is_byte_exact() {
        assert_eq!(
            NOT_ENABLED_ERROR,
            "Directory listing is not enabled in this build."
        );
        let r = dir_error(NOT_ENABLED_ERROR.to_string());
        assert_eq!(
            r.model_content.as_deref(),
            Some("Directory listing is not enabled in this build.")
        );
        assert_eq!(r.data["resources"], json!([]));
        assert!(!r.is_error);
    }

    /// `FSf`'s three conjuncts: non-null OBJECT under the extension key, the
    /// `directoryRead` key present, and its value exactly `true`.
    #[test]
    fn directory_read_predicate_matches_fsf() {
        let yes = json!({"extensions":{"io.modelcontextprotocol/skills":{"directoryRead":true}}});
        assert!(server_declares_directory_read(Some(&yes)));

        for no in [
            // key absent entirely
            json!({}),
            // extensions present, our key absent
            json!({"extensions":{"other":{"directoryRead":true}}}),
            // entry is not an object
            json!({"extensions":{"io.modelcontextprotocol/skills":true}}),
            json!({"extensions":{"io.modelcontextprotocol/skills":null}}),
            // object without the key
            json!({"extensions":{"io.modelcontextprotocol/skills":{}}}),
            // TRUTHY but not `true` — `t.directoryRead===!0` rejects these.
            json!({"extensions":{"io.modelcontextprotocol/skills":{"directoryRead":1}}}),
            json!({"extensions":{"io.modelcontextprotocol/skills":{"directoryRead":"yes"}}}),
            json!({"extensions":{"io.modelcontextprotocol/skills":{"directoryRead":false}}}),
        ] {
            assert!(
                !server_declares_directory_read(Some(&no)),
                "must reject {no}"
            );
        }
        assert!(!server_declares_directory_read(None));
        assert_eq!(MCP_SKILLS_EXTENSION_KEY, "io.modelcontextprotocol/skills");
    }

    /// The SUCCESS mapper: `${name}${mimeType===JBn?"/":""}` lines under a
    /// pluralised count header, then a blank line, then the compact JSON of the
    /// whole result object.
    #[test]
    fn success_listing_matches_the_oracle_mapper() {
        let entries = vec![
            mcp::McpDirectoryEntry {
                uri: "res://a".into(),
                name: "a.md".into(),
                mime_type: Some("text/markdown".into()),
            },
            mcp::McpDirectoryEntry {
                uri: "res://sub".into(),
                name: "sub".into(),
                mime_type: Some(DIRECTORY_MIME_TYPE.into()),
            },
        ];
        let r = dir_listing(entries);
        let mc = r.model_content.as_deref().unwrap();
        assert!(
            mc.starts_with("Directory listing (2 entries):\na.md\nsub/\n\n{"),
            "got {mc}"
        );
        // …and the tail is the compact JSON of `{resources:[…]}`.
        assert!(mc.contains("\"resources\":["));
        assert!(
            !mc.contains("\n  "),
            "Ie(e) takes NO indent on the success branch"
        );
        assert!(!r.is_error);

        // Singular + empty forms.
        let one = dir_listing(vec![mcp::McpDirectoryEntry {
            uri: "res://a".into(),
            name: "a.md".into(),
            mime_type: None,
        }]);
        assert!(one
            .model_content
            .as_deref()
            .unwrap()
            .starts_with("Directory listing (1 entry):\na.md\n\n{"));
        let empty = dir_listing(Vec::new());
        assert!(empty
            .model_content
            .as_deref()
            .unwrap()
            .starts_with("Directory is empty.\n\n{"));
    }

    /// The input schema uses the ORACLE's `server`/`uri` keys.
    #[test]
    fn input_schema_uses_the_oracle_key_names() {
        let props = &INPUT_SCHEMA["properties"];
        assert!(props.get("server").is_some(), "oracle key is `server`");
        assert!(props.get("uri").is_some());
        assert!(
            props.get("server_name").is_none(),
            "`server_name` is the port's pre-existing divergence on the two sibling tools; \
             a freshly ported tool matches the oracle"
        );
    }

    #[tokio::test]
    async fn resolve_server_accepts_a_cached_resource_server() {
        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior::default(),
        )
        .await;

        let resolved = resolve_server(&registry, "cached")
            .await
            .expect("cached server counts as connectable");
        assert_eq!(resolved, "cached");
    }

    #[tokio::test]
    async fn resolve_server_ignores_scoped_only_entries() {
        let (registry, _transport) = new_cached_registry().await;
        let agent_id = lingxi_core::types::AgentId::new();
        let scoped_key = mcp::registry::agent_scope_table_key(agent_id, "cached");
        registry.connections.write().await.insert(
            scoped_key,
            mcp::McpConnectionState::Cached {
                config: crate::mcp_tool::cached_resource_test_support::cached_server_config(
                    "cached",
                ),
                connection_id: lingxi_core::types::McpConnectionId::new(),
                capabilities: resource_caps(true),
                negotiated: lingxi_core::host::McpNegotiatedProtocol {
                    era: lingxi_core::host::McpProtocolEra::Legacy,
                    version: "2025-11-25".into(),
                },
                tools: vec![],
                resources: vec![],
                resource_templates: vec![],
                prompts: vec![],
                cache_saved_at_ms: 0,
                age_ms: 0,
            },
        );

        let err = resolve_server(&registry, "cached")
            .await
            .expect_err("scoped-only row must not satisfy shared lookup");
        assert!(format!("{err}").contains("Server \"cached\" not found"));
    }

    #[tokio::test]
    async fn cached_server_directory_listing_lazy_dials_after_gate() {
        let _flag = ScopedMcpSkillsFlag::set(true);

        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior {
                cached_capabilities: resource_caps(true),
                live_capabilities: resource_caps(true),
                directory_entries: vec![mcp::McpDirectoryEntry {
                    uri: "cached://docs/readme.md".into(),
                    name: "readme.md".into(),
                    mime_type: Some("text/markdown".into()),
                }],
                ..Default::default()
            },
        )
        .await;

        let tool = ReadMcpResourceDirTool::new(ctx_with_registry(registry));
        let result = tool
            .call(
                json!({ "server": "cached", "uri": "cached://docs" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("cached directory read succeeds");

        assert_eq!(
            result.data["resources"],
            json!([{
                "uri": "cached://docs/readme.md",
                "name": "readme.md",
                "mimeType": "text/markdown"
            }])
        );
        assert_eq!(transport.connect_calls("cached"), 1);
        assert_eq!(
            transport.rpc_call_count("cached", "resources/directory/read"),
            1
        );
    }

    #[tokio::test]
    async fn cached_server_directory_exact_raw_name_beats_normalized_collision() {
        let _flag = ScopedMcpSkillsFlag::set(true);

        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "my.server",
            CachedServerBehavior {
                cached_capabilities: resource_caps(true),
                live_capabilities: resource_caps(true),
                directory_entries: vec![mcp::McpDirectoryEntry {
                    uri: "cached://dot/readme.md".into(),
                    name: "readme.md".into(),
                    mime_type: Some("text/markdown".into()),
                }],
                ..Default::default()
            },
        )
        .await;
        seed_cached_server(
            &registry,
            &transport,
            "my_server",
            CachedServerBehavior {
                cached_capabilities: resource_caps(true),
                live_capabilities: lingxi_core::host::ServerCapabilitiesDto {
                    directory_read: false,
                    ..resource_caps(false)
                },
                directory_entries: vec![mcp::McpDirectoryEntry {
                    uri: "cached://underscore/should-not-hit".into(),
                    name: "wrong.md".into(),
                    mime_type: Some("text/markdown".into()),
                }],
                ..Default::default()
            },
        )
        .await;

        let result = ReadMcpResourceDirTool::new(ctx_with_registry(registry))
            .call(
                json!({ "server": "my.server", "uri": "cached://dot" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("exact raw dot name should keep dot server across dial and RPC");

        assert_eq!(
            result.data["resources"],
            json!([{
                "uri": "cached://dot/readme.md",
                "name": "readme.md",
                "mimeType": "text/markdown"
            }])
        );
        assert_eq!(transport.connect_calls("my.server"), 1);
        assert_eq!(transport.connect_calls("my_server"), 0);
        assert_eq!(
            transport.rpc_call_count("my.server", "resources/directory/read"),
            1
        );
        assert_eq!(
            transport.rpc_call_count("my_server", "resources/directory/read"),
            0
        );
    }

    #[tokio::test]
    async fn cached_server_directory_ambiguous_normalized_alias_fails_closed_without_rpc() {
        let _flag = ScopedMcpSkillsFlag::set(true);

        let (registry, transport) = new_cached_registry().await;
        for name in ["my.server", "my_server"] {
            seed_cached_server(
                &registry,
                &transport,
                name,
                CachedServerBehavior {
                    cached_capabilities: resource_caps(true),
                    live_capabilities: resource_caps(true),
                    ..Default::default()
                },
            )
            .await;
        }

        let err = ReadMcpResourceDirTool::new(ctx_with_registry(registry))
            .call(
                json!({ "server": "my/server", "uri": "cached://ambiguous" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect_err("colliding normalized alias must fail closed");
        let err_text = format!("{err}");
        assert!(err_text.contains("ambiguous"));
        assert!(err_text.contains("my.server"));
        assert!(err_text.contains("my_server"));
        assert_eq!(transport.connect_calls("my.server"), 0);
        assert_eq!(transport.connect_calls("my_server"), 0);
        assert_eq!(
            transport.rpc_call_count("my.server", "resources/directory/read"),
            0
        );
        assert_eq!(
            transport.rpc_call_count("my_server", "resources/directory/read"),
            0
        );
    }

    #[tokio::test]
    async fn cached_server_directory_listing_fails_closed_when_live_bit_is_false() {
        let _flag = ScopedMcpSkillsFlag::set(true);

        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior {
                cached_capabilities: resource_caps(true),
                live_capabilities: resource_caps(false),
                directory_entries: vec![mcp::McpDirectoryEntry {
                    uri: "cached://docs/readme.md".into(),
                    name: "readme.md".into(),
                    mime_type: Some("text/markdown".into()),
                }],
                ..Default::default()
            },
        )
        .await;

        let result = ReadMcpResourceDirTool::new(ctx_with_registry(registry))
            .call(
                json!({ "server": "cached", "uri": "cached://docs" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("live false bit returns model-facing error");

        assert_eq!(
            result.data,
            json!({
                "resources": [],
                "error": "Server \"cached\" does not support directory listing."
            })
        );
        assert_eq!(transport.connect_calls("cached"), 1);
        assert_eq!(
            transport.rpc_call_count("cached", "resources/directory/read"),
            0
        );
    }

    #[tokio::test]
    async fn cached_server_directory_listing_rechecks_live_resources_before_rpc() {
        let _flag = ScopedMcpSkillsFlag::set(true);

        let (registry, transport) = new_cached_registry().await;
        seed_cached_server(
            &registry,
            &transport,
            "cached",
            CachedServerBehavior {
                cached_capabilities: resource_caps(true),
                live_capabilities: lingxi_core::host::ServerCapabilitiesDto {
                    resources: false,
                    directory_read: true,
                    ..resource_caps(false)
                },
                directory_entries: vec![mcp::McpDirectoryEntry {
                    uri: "cached://docs/readme.md".into(),
                    name: "readme.md".into(),
                    mime_type: Some("text/markdown".into()),
                }],
                ..Default::default()
            },
        )
        .await;

        let result = ReadMcpResourceDirTool::new(ctx_with_registry(registry))
            .call(
                json!({ "server": "cached", "uri": "cached://docs" }),
                tool_api::test_support::fresh_ctx(),
                tool_api::test_support::fresh_tx(),
            )
            .await
            .expect("live resources downgrade returns model-facing error");

        assert_eq!(
            result.data,
            json!({
                "resources": [],
                "error": "Server \"cached\" does not support resources"
            })
        );
        assert_eq!(transport.connect_calls("cached"), 1);
        assert_eq!(
            transport.rpc_call_count("cached", "resources/directory/read"),
            0
        );
    }
}
