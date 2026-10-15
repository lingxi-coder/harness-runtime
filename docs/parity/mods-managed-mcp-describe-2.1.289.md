# Managed MCP sign-in tool descriptions in Claude Code 2.1.289

This note traces the 2.1.289 rule that protects descriptions of organization-
managed MCP sign-in tools from user-installed Mods. The pinned executable is
`/tmp/harness-claude-code-2.1.289-darwin-arm64`, SHA-256
`03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69`.

Run the pure-helper probe with:

```sh
node scripts/tests/mods_managed_mcp_describe_289_probe.mjs \
  /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The probe verifies the executable and each source slice, then executes the
source-pinned configured-scope projection, MCP provider helpers, and
`sec-default` description router with controlled plugin-registry and `next`
callbacks. It does not start Claude Code, an OAuth flow, an MCP server, or the
Native plugin dispatcher. The OAuth placeholder constructors are checked as
source only because they depend on live session and auth services.

## Native classification and routing

The sign-in placeholders are real per-server tools. Native builders `G` and
`V` create the `authenticate` and `complete_authentication` tool names, set
`isMcp: true`, and attach `mcpInfo` containing `serverName`, `toolName`,
`serverType`, `source: Tj(server, config)`, and `isAuthStub: true`. Their own
`description()` methods return the sign-in instructions. `Tj` returns the
configured scope for ordinary non-plugin MCP servers; it returns `"plugin"`
for plugin-sourced or plugin-owned dynamic servers. Thus a managed or
enterprise auth placeholder carries that scope in `mcpInfo.source` even though
the placeholder does not set `mcpInfo.scope` itself.

At description time, `sFn` recognizes an MCP tool, gets the server name from
`mcpInfo` (or its `mcp__server__tool` name), and reads any installed-plugin
server record through `zLo`. It passes `mcpInfo.scope ?? mcpInfo.source` and
the plugin source to `hQ`. `hQ` classifies the exact scopes `"managed"` and
`"enterprise"` as `{plugin: "mcp:<server>", tier: "prepend"}`. Otherwise it
uses the owning plugin's tier if one exists, and falls back to a `user`-tier
MCP provider. If the plugin-record read rejects, `sFn` logs that it is treating
the provider as the organization's and returns `prepend`; a missing record
that resolves normally is different and proceeds through `hQ`.

`lFn` sends that provider with the current tool name and description to the
`tool.describe` hook. The Native argument contract pins the provider and tool
identity, while requiring only that the returned description is a string.
This prevents a hook from changing a managed provider to `user` while still
allowing ordinary description rewrites.

The host-owned `sec-default` hook is seated outermost. For `tool.describe` it
calls `d(event, next)`: provider tiers `user`, `builtin`, and `core` use normal
`next(event)`; every other tier uses `next.to(event, "append")`. Consequently,
the managed/enterprise provider (`prepend`) bypasses user-installed hooks and
continues in the append layer, while an ordinary user MCP provider continues
through the normal chain. This is the scoped rule: it protects the
organization-managed provider path without disabling description hooks for
ordinary tools.

The actual Native hook dispatcher confirms what that target means. Its tier
order is `prepend`, `user`, `append`, `builtin`, `core`. A prepend hook may
continue at `append`; the `next.to` adapter wraps that target and `Fn` builds a
floor from `prepend` to `append`. `Nas` matches only tiers strictly between
those endpoints, and the dispatcher uses `Mn` to skip the matching hook. Thus
the user tier is bypassed, while append/builtin/core continue. The probe
executes the exact floor builder and matcher for this case and verifies that
the user-tier owner is bypassed but the append-tier owner is not.

`tool.register` is a separate operation. The Native `sec-default` registration
handler has its own tier and `allowedMcpServers` policy. It must not be used as
a proxy for the `tool.describe` provider decision. The pinned executable has
the relevant hook names `tool.describe` and `tool.register`; searches for the
literal hook/API names `tool.add` and `tool.define` returned no match.

The Native `r8`/`Nit` path excludes the built-in `sec-default` module from the
“other description hook is present” measurement. `lFn` uses this for a
diagnostic if the description changes unexpectedly; it is not the mechanism
that filters a user hook. The filtering mechanism is the provider tier plus
`next.to(..., "append")`.

## Pinned ranges

All ranges are half-open byte offsets in the pinned arm64 executable. The
probe checks each SHA-256 before inspecting or executing the source.

| Native source | Byte range | SHA-256 |
| --- | ---: | --- |
| `$st`, `gQ`, and `hQ` managed/enterprise provider classification | `187631796..187632304` | `ab6d301969e0561b317a782857d37ff63080cbacddade53791010f349530e4f4` |
| `zLo` installed-plugin MCP record lookup | `187689722..187689937` | `9087feb66f578a56a77b8a4cbe7a1292d2a818e534657b8d20e9bb3339191380` |
| `sFn` MCP tool provider resolver | `187700969..187701489` | `8a3f3a585106e02d5299a74dd529a08b62bb5479d86d125448e242a48a0309ae` |
| `Nit`, `lFn`, `tool.describe` call and change measurement | `187701489..187702322` | `7223a00d2215b01f84a8781171710dd19c1452a08071aa7b13ebbfa878e4af2b` |
| `r8` excludes `sec-default` from hook-presence measurement | `187121157..187121353` | `99549cd0da8db075981557fc1e708d9d94099cf67d4b6d9febe81005aa3fa9b6` |
| `tool.describe` provider pin and schema | `182736910..182737832` | `8cc850d27cf57ed5cb62a3cfc64776e009adccd68dd0684e0c488bb4c86ede02` |
| Hook tier order, `ESt`, `Nas`, `Fas`, and permitted targets | `182598362..182599150` | `cde7a69c3c700a68abe5bee413b0a15033254d88da634250524584815bd19540` |
| `Fn` validation and next-tier floor construction | `182766661..182767293` | `203c737b352a7c57c8dc478c1642278766e944a8f38df5369c5e391557f443b1` |
| `By` applies the target floor to the continuation | `182769578..182770499` | `1590dbaa7f986d05d3cb7337d32acbce847670f8a4b3d94c7a3773090c6d5008` |
| `Mn` skips handlers between floor endpoints | `182763890..182764378` | `5ebc572353233a62b8dfef90b82db03316ac9a8e41535dfb08b4a437719ac9b8` |
| `Tj` config-scope/source projection | `187741909..187742266` | `247a6d92cf8a6689d960ae6ceea5b1beb31797af70f1addf032d71ae4778244e` |
| `ML` plugin-source predicate used by `Tj` | `179781389..179781436` | `6b27b1085141d98c0cfe644f4ccceb7c8b3ea248ffcbfeedbcc6a7c7cd84d8b8` |
| `sec-default` provider-tier router `d` | `213012004..213012125` | `2099f21dce425f816c22308841153667fe2b6ac9ba291f8d25d941f1b87ba20a` |
| `sec-default` `tool.describe` registration | `213013622..213013657` | `1535cc5b3906e8d91f807c1f31c81793450358f34d1eebfa6f4d7338dff2606e` |
| separate `sec-default` `tool.register` handler | `213013761..213013957` | `53401a0fd27d6d048891f0253480bff984f22ffe5b17337a95ee55893c696744` |
| `sec-default` outer-seat description | `213014666..213015201` | `8f495167f46b52193c2cdeb1188a09df05915b317e1bf263a4bd5a15b270a7e0` |
| Native per-server OAuth `authenticate` placeholder builder `G` | `205914815..205919979` | `dd944f83c9c8029eec952619239d17b79970aea407e0888d3d2c4db002b24b6a` |
| Native per-server OAuth `complete_authentication` placeholder builder `V` | `205919979..205923097` | `0845d93d147661dca2ec7964ad5b494809b4edc53f0b65d82ba65ef559e1566e` |

The probe's executed helper cases produced:

| MCP config scope | `Tj` source | `hQ`/`sFn` provider tier |
| --- | --- | --- |
| `managed` | `managed` | `prepend` |
| `enterprise` | `enterprise` | `prepend` |
| `user` | `user` | `user` |
| `project` | `project` | `user` |
| `dynamic`, with no plugin owner | `dynamic` | `user` |
| user-scoped MCP record read rejects | not used after rejection | `prepend` fail-closed path |

The actual `d` helper was also executed with a recording `next`: `prepend` and
`append` requested `next.to(event, "append")`; `user`, `builtin`, and `core`
requested ordinary `next(event)`. The callback's synthetic “rewritten” text
only demonstrates which continuation the helper selected; it is not a claim
that the Native hook dispatcher was launched.

## Harness production path and gap

The existing Harness already supplies part of the right identity. In
[`tooling.rs`](../../crates/orchestrator/src/conversation/tooling.rs),
`mod_tool_provider` derives the MCP server from a bound `mcp__<server>__<tool>`
name and emits `tier: "prepend"` for `ConfigScope::Managed`,
`ConfigScope::Enterprise`, and an unreadable/missing server config. It emits
`tier: "user"` for other known MCP scopes. `apply_mod_tool_descriptions`
passes that provider to `tool.describe` for each model-facing tool and accepts
the returned description. The worker pins the forwarded `tool` and `provider`
but intentionally accepts a changed string description, so ordinary rewrites
work.

The `sec-default` `tool.describe` route is now implemented in the worker's
synthetic host-owned handler. It is injected at the same managed seat as the
other `sec-default` events and applies the Native provider-tier router:
`user`/`builtin`/`core` continue with `next(event)`; other tiers continue with
`next.to(event, "append")`. A real `ModHost` regression uses a user Mod and an
append Mod: managed and enterprise provider inputs skip the user's rewrite but
retain the append-layer result, while `user`, `builtin`, and `core` inputs keep
the normal user rewrite followed by the append rewrite. The Hooks Rust test
awaits Root's serial Cargo validation.

There is a separate producer mismatch for the exact Native sign-in tools.
Harness currently has a per-tool `MCPTool` whose displayed description comes
from the configured MCP tool, plus one global `McpAuth` inspection tool. It
does not construct Native's per-server `authenticate` and
`complete_authentication` OAuth placeholder tools or carry their `isAuthStub`
marker. The existing provider tier can protect a future per-server auth tool
once it uses the real server scope; it does not establish that this producer
exists today.

The production routing slice is confined to the synthetic `sec-default`
dispatch in `crates/hooks/src/mods_worker.mjs` and the focused real-`ModHost`
regression in `crates/hooks/src/mods.rs`. It leaves the existing
`tool.describe` tool/provider identity pin intact and preserves ordinary
user-provider rewrites. The per-server OAuth placeholder producer remains an
independent gap; this routing change does not add `authenticate` or
`complete_authentication` tools to Harness.

Current Harness references:

- [`tooling.rs`](../../crates/orchestrator/src/conversation/tooling.rs:807) derives provider tier; lines 842 onward dispatch and apply `tool.describe` answers.
- [`mods_worker.mjs`](../../crates/hooks/src/mods_worker.mjs:45) declares the event and tier order; lines 53–60 define continuation targets; lines 3267–3283 enumerate synthetic `sec-default` events; the `secDefaultToolDescribe` helper routes by provider tier; lines 1001–1012 pin provider identity while permitting description strings; lines 3711–3719 enforce `.to()` targets.
- [`mods.rs`](../../crates/hooks/src/mods.rs:12961) exercises managed/enterprise bypass, append-layer continuation, and user/builtin/core controls through the real ModHost worker.
- [`mcp_tool.rs`](../../crates/tools/mcp/src/mcp_tool.rs:1359) defines the current per-tool MCP wrapper and `is_mcp`; lines 1531–1538 use the server description. `McpAuthTool` is a single generic inspector at lines 2041–2075.
- [`mods.rs`](../../crates/hooks/src/mods.rs:6045) reserves the host-owned `cc-plugin-sec-default@builtin` storage identity; its Rust API origin registration is not itself a `tool.describe` event handler.

This finding is specific to description filtering. It does not justify
disabling all `tool.describe` hooks, changing `tool.register`, or treating an
organization `allowedMcpServers` list as the source of MCP provider identity.
