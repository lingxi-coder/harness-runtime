# Per-server MCP authentication stubs: implementation map for 2.1.289

This note records the current Harness producer and a dependency-complete
implementation slice for the Native per-server `authenticate` and
`complete_authentication` tools. It supplements
[`mods-managed-mcp-describe-2.1.289.md`](mods-managed-mcp-describe-2.1.289.md),
whose scope is the `tool.describe` provider decision.

The pinned Native executable is
`/tmp/harness-claude-code-2.1.289-darwin-arm64`, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.
All Native ranges below are half-open byte offsets in that executable. The
builder and call-site findings are source-level evidence; the existing probe
does not execute OAuth, construct the stubs, or dispatch a real MCP server.

## Native producer and tool contract

The auth-stub pair is selected for a client in the `needs-auth` state. The
`fD` producer returns no stubs when the host is non-interactive; otherwise it
builds both server-bound tools. The state consumer calls `fD` for
`client.type === "needs-auth"`.

| Native source | Byte range | SHA-256 |
| --- | ---: | --- |
| `fD` interactive pair producer | `205914584..205914674` | `9cb5de0274f76071f0d2114e8986e37900fe90444a5fa249181712e7ebf976ab` |
| host `isInteractive()` predicate used by `fD` | `179307203..179307263` | `c5962d665b6699f61261e7a0bccf7d54659c6deee200a6725eeb6402707056f1` |
| `needs-auth` state consumer installing the pair | `206358320..206358416` | `004035ebeacf6a0f0da5cd4b4823e6007eaf41e7526c5cd955a1467a61126515` |
| `authenticate` builder `G` | `205914815..205919979` | `dd944f83c9c8029eec952619239d17b79970aea407e0888d3d2c4db002b24b6a` |
| `complete_authentication` builder `V` | `205919979..205923097` | `0845d93d147661dca2ec7964ad5b494809b4edc53f0b65d82ba65ef559e1566e` |
| input schema helpers `B` and `x` | `205914173..205914362` | `cbc088a86cad721df75d3f79f22676aa8719f4b741c4c3ce973f964a99924cb1` |
| optional claude.ai `serverId` helper `r_e` | `187653780..187653874` | `71ce80f6dd013bfcb5853c24a2656b86d6f12e624a7bf97ffd0a1d2385882bd6` |
| configured source projection `Tj` | `187741909..187742266` | `247a6d92cf8a6689d960ae6ceea5b1beb31797af70f1addf032d71ae4778244e` |

Both names are built with the Native MCP FQN helper: `mcp__<server>__authenticate`
and `mcp__<server>__complete_authentication`. Both tools set `isMcp: true`,
`isAuthStub: true`, `serverName`, their corresponding `toolName`, the
configured `serverType`, and `source: Tj(server, config)`. `serverId` is
conditionally present only when the config is a `claudeai-proxy` scoped to
`claudeai` and has an id; `r_e` otherwise returns undefined, so the property
is omitted from serialized metadata.

`authenticate` uses the empty object schema `B() = z.object({})`. Its
description and prompt say that the named server is installed but needs
authentication, ask the model to start OAuth and share the returned URL, and
say the server's real tools become available after authorization. The server
type is displayed with any Native origin detail available to the builder.
`complete_authentication` uses `x() = z.object({ callback_url: z.string() })`;
the property description asks for the complete callback URL from the browser
address bar. Its description says to call `authenticate` first, then submit the
redirect URL. Both are enabled, non-concurrency-safe, and not read-only.

The `authenticate` call has per-server guard branches for managed-policy
block, disabled server, project approval, claude.ai proxy, unsupported
transport, and hosted Anthropic cases. For a supported server it begins or
reuses that server's OAuth flow and returns an `auth_url` result; it does not
wait for the user's browser before returning. The Native source also checks an
existing attempt against config, account token, identity epoch, callback
submitter, and age before reusing it. These are source-observed Native checks;
any Harness generation token or locking mechanism is an implementation choice
unless independently pinned to an equivalent Native fact.

`complete_authentication` looks up the in-progress callback submitter for the
same server. It returns a structured error when no flow exists, when the URL
has neither an authorization code nor provider error, or when the callback
state belongs to a different attempt. A valid callback is submitted to the
active flow, which is awaited before returning success or failure.

## Current Harness producer and auth flow

The current per-tool model-facing catalog is built by
[`build_registered_mcp_tools`](../../crates/tools/mcp/src/mcp_tool.rs). It
matches only `Connected` and `Cached` registry states, and creates one
`MCPTool` per `McpToolDto` using its `full_name`, description, input/output
schemas, search hint, `always_load`, and `requires_user_interaction`. It then
applies per-tool permission ceilings and server role. It does not create tools
for `NeedsAuth` or `AwaitingOAuth` states.

Instead, Desktop's [`register_all`](../../crates/tools/mcp/src/lib.rs) adds one
global (one-per-runtime, not one-per-server) `McpAuthTool` builtin to the
model's tool list; Mobile does not call this base-tool registration. That tool
is always enabled, accepts `{server_name: string}`, and describes itself as inspecting the
configured auth/transport surface. Its `call` reads `McpServerConfig` and
returns transport kind, inferred auth kind, client id, callback port,
metadata URL, and header/helper booleans. It neither examines the current
connection's authenticated state nor begins/completes OAuth. Its `auth_kind`
is inferred from the static transport config. The `is_mcp_wire_tool` helper in
`orchestrator/src/handle_impl.rs` also recognizes the global `McpAuth` name.
This is an unnative model-facing inspection alias; it is not the Native auth
stub pair. Remove it from model-facing registration/accounting when the
per-server tools are implemented, rather than keeping it as a compatibility
tool. The host `/mcp` status and management command remains a separate
surface.

`McpServerConfig.scope` is already retained in every `McpConnectionState` and
has the source needed for auth tool metadata. It covers settings scopes,
Enterprise, Dynamic, ClaudeAi, and Agent. The Native `Tj` projection uses the
configured scope for ordinary servers and `plugin` for plugin-sourced or
plugin-owned dynamic servers. Harness plugin configs are namespaced as
`plugin:<plugin>:<server>`, keep `scope = Dynamic`, and are marked
`metadata.agent_source = Plugin` by `PluginManager`; the metadata enum does
not itself contain the owning plugin identity.

`ConversationTooling::mod_tool_provider` derives an MCP server from the
`mcp__<server>__<tool>` name and reads its config. It returns the `mcp:<server>`
provider with `prepend` for Managed/Enterprise and `user` for other known
scopes (and fail-closed `prepend` if the server config is unavailable). It
does not currently use `metadata.agent_source` to recover plugin ownership or
the owning plugin's tier. A per-server auth tool must preserve this source
identity so its `tool.describe` provider follows the same native source rule;
using only `Dynamic` would flatten a plugin source to the user tier.

The current registry auth flow is implicit and blocking. `McpRegistry::connect`
and `connect_all` drive `resolve_oauth_spec`; when an OAuth config has no usable
token, `run_interactive_oauth` sets `AwaitingOAuth`, invokes the host's
`OnAuthorizationUrl`, waits for the loopback callback (bounded to five
minutes), persists tokens, then proceeds to connect and discover tools.
`call_tool_with_auth_retry` can also reconnect after an auth response and
trigger refresh or a new interactive flow. Public lower-level
`perform_oauth_flow` and `perform_oauth_flow_with_manual_input` both await the
whole flow; the latter reads a pasted URL/code from an `AsyncBufRead`. There is
no per-server begin/complete API that returns a URL and later submits a
callback URL from a tool call. `McpAuthTool` is not that API.

The MCP catalog-change broadcast is for catalog invalidation/list changes.
Desktop and mobile rebuild the dynamic MCP partition from
`build_registered_mcp_tools` on those events. It does not represent all
connection-state transitions, so it cannot by itself publish an auth pair
when a later connection settles in `NeedsAuth`.

## Dependency-complete implementation slice

1. **Own the pending OAuth attempt in the MCP registry/auth layer.** Split the
   existing full-flow path into a per-server begin operation that obtains and
   returns the authorization URL promptly, plus a callback-submit/complete
   operation that validates the callback against that attempt and awaits token
   exchange. Reuse the existing OAuth metadata discovery, PKCE, secure storage,
   `OnAuthorizationUrl`, and callback listener. A pasted callback must enter
   the exact waiting flow; `parse_manual_callback_input` alone is not a
   completion API. Do not hold the per-server lifecycle lock while waiting for
   user action. After success, revalidate the current config and eligibility,
   persist the grant, and use the existing connection/catalog path.
2. **Construct server-bound tools from auth state.** Add distinct tool
   implementations with the names, schemas, descriptions, `is_mcp` behavior,
   and `isAuthStub`-equivalent metadata above. Gate pair creation on the real
   interactive-session fact. Build from the registry's preserved config source
   and the needs-auth server state; do not synthesize a transport
   `McpConnectionId` for a disconnected server.
3. **Give auth tools a real dynamic ownership key.** The current
   `ToolRegistry` MCP partition is keyed by `McpConnectionId`, which is only
   valid for a connection. Add a server-owned auth partition or a typed MCP
   partition key that can represent `AuthServer(server_name)` as well as
   `Connected(connection_id)`. Replace the per-server auth tools atomically
   with the discovered real tools after successful connection; remove them on
   disable/remove/config replacement. Preserve tool ordering and the existing
   Connected/Cached per-tool behavior.
4. **Publish auth-state changes to desktop and mobile.** Add a registry
   notification for relevant server-state changes (especially NeedsAuth,
   AwaitingOAuth, Connected, disabled, removed) and have both Runtime refresh
   the combined MCP tool snapshot. Initial construction already snapshots
   after initial `connect_all`; this event covers plugin/settings reconnects
   and failures after the shared `ToolRegistry` exists.
5. **Preserve source for `tool.describe`.** Keep the configured settings/managed
   scope and plugin source/owner attached to the auth stub. Managed/Enterprise
   remains the Native organization provider; plugin-owned dynamic servers must
   resolve through plugin provenance rather than falling through to generic
   user MCP. This source fact is separate from `tool.register` policy.
6. **Retire the global alias.** Remove `McpAuthTool` from `register_all` and
   the `McpAuth` model-tool classification once the per-server producer is
   installed. Audit its dedicated tool telemetry and list snapshot references
   for dead-code cleanup; do not remove independent `/mcp` host status,
   reconnect, enable, or disable behavior.

## Acceptance boundaries

- A source-pinned test verifies the pair's exact names, schemas, descriptions,
  source metadata, and interactive omission. A Harness unit/integration test
  verifies that a NeedsAuth server contributes only its own pair, a successful
  callback replaces that pair with discovered tools, and disable/remove/config
  replacement retires the pair.
- Callback tests use controlled OAuth HTTP/storage/callback seams, not a live
  provider: no-flow, malformed callback, wrong state, cancellation/failure,
  and success each have explicit results. The successful test also proves the
  grant is stored and the shared MCP tool snapshot changes.
- A `tool.describe` test covers Managed/Enterprise, user, and plugin-owned MCP
  auth stubs, including the provider tier and plugin source. The source-specific
  Native routing decision in the companion parity note remains authoritative.
- The existing Native probe checks the constructors as source only. No claim is
  made here that Native OAuth ran in a live process, that the source-only
  constructors have been VM-executed, or that a new Harness API is already
  implemented. No OAuth or network request was made for this map.
