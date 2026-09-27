# mcp / plugin re-audit — 2.1.251 → 2.1.267

The mcp/plugin subsystem was last aligned against **2.1.251** (2026-08-28…31),
sixteen releases back — the largest baseline gap in the tree. This is the
re-audit.

**Oracles, both verified.** New: `~/.local/share/claude/versions/2.1.267`,
sha256 `a681f3008f0050029aeebcab3af51bb6a55ddeb625a3af3141a4416d43cd2558`.
Old: 2.1.251, refetched with
`npm pack @anthropic-ai/claude-code-darwin-arm64@2.1.251`, sha256
`625869b01e0050f260b2980fac248fd9cef9e462612bded4ec9d3d49ff8969a5` — which
**matches the value `docs/REMAINING.md` recorded**, so the old side is the same
binary the 251 audit used. Tree at `13d73b2a8`.

---

## Method, and its self-check

Naive cross-version string diff over the two binaries yielded **198**
mcp/plugin candidates. Raw-substring re-verification of each candidate's
longest placeholder-free fragment against 2.1.251's **bytes** dropped 45 of
them — a **23% false-positive rate**, in line with the 36% measured on a
263→267 sample. 153 survived as genuinely new.

Port presence was then checked against a 16 MB concatenation of the port's
mcp/plugin-adjacent Rust sources, trying both the raw and Rust-escaped
spellings. That returned **0 of 153 present** — a 100% miss is the signature of
a broken comparison, so before believing it I probed the haystack with four
identifiers known to be in the port: `requiresUserInteraction` (16),
`resource_metadata` (55), `allowManagedMcpServersOnly` (17),
`skipMcpDiscovery` (8). The haystack is sound; the miss is real, and expected
for a port pinned at 251.

🚨 **153 is an upper bound for scoping, not a backlog.** Classified:

| bucket | count | disposition |
|---|---|---|
| MCP connection / transport / auth | 45 | **the actionable bucket** — see below |
| marketplace / plugin CLI copy | 21 | mostly copy, low risk |
| SDK / connector / claude.ai | 12 | accepted divergence |
| function-hook runtime | 6 | classified, not to build |
| plugin surface modules / JSX | 6 | new in 267, classified, not to build |

---

## MCP-01 — no legacy HTTP+SSE fallback on a rejected `initialize` — ✅ FIXED in `a1e070f4e`

Upstream dials streamable HTTP, and when the `initialize` POST is rejected it
**re-dials the server over legacy HTTP+SSE** (`src_187489883.js` @63137,
mirrored at `src_187282055.js` @63512):

```js
try { await se(L, P, Xl()) }
catch(A){
  if (I===void 0 || P instanceof vmt && P.protocolVersion!==void 0
      || !en(A) || !H("tengu_mcp_legacy_sse_fallback",!0)) throw A;
  J(e, `initialize POST rejected (…); trying legacy HTTP+SSE`);
  let k = A.code === 405;
  await P.close().catch(()=>{});
  … // re-dial, passing { postMethodNotAllowed: k, dialSignal, dialConnected }
}
```

The predicate is precise, and identical in both chunks:

```js
function Hs(e){
  if(!(e instanceof o_) || (e.status!==400 && e.status!==404 && e.status!==405)) return !1;
  let n = e.data.text;
  if(typeof n !== "string") return !0;
  return !js(n);              // …and the body is NOT a JSON-RPC message
}
```

So the fallback fires on a **400/404/405 whose body does not parse as JSON-RPC**
(`js` tolerates SSE framing by reading the `data:` line). A server that returned
a *valid JSON-RPC error* over the rejected POST is a real protocol failure and
must NOT be re-dialled — getting that half wrong would convert protocol errors
into silent transport churn.

**Port:** absent. `McpClient::initialize` (`mcp/src/client.rs:754`) maps any
error straight to `McpClientError::Rpc` with no fallback; `legacy_sse`,
`postMethodNotAllowed` and any 400/404/405 branch are 0 hits in `mcp/`.
`tengu_mcp_legacy_sse_fallback` is 0 hits (oracle default **true**, so this is
live upstream behaviour, not a dormant flag).

**Impact:** an MCP server that speaks only HTTP+SSE fails to connect here and
connects upstream.

**Fixed in `a1e070f4e`**, on top of the prerequisite in `ff8bda8b4`. The
blocker below was measured before implementation and turned out to be avoidable:
siting the rescue inside `RemoteMcpTransport::connect_and_initialize` rather than
the registry retry seam removed almost all of it. See "How it was actually done".

**Blocker as originally measured:**

* `McpError::HttpResponse { status, www_authenticate }` carries the **status but
  not the response body**, and the predicate needs both. Widening it is additive
  in spirit but enum struct variants take no `..Default::default()`, so all
  **7** construction sites must change, in `platform-api` — a type the whole
  workspace consumes.
* There are **8** `connect_and_initialize` implementations (posix, common,
  windows, mobile, desktop, plus test doubles); only the ones that can see a
  body would populate it.
* Upstream hands the SSE re-dial a small dial-state protocol —
  `{postMethodNotAllowed, dialSignal, dialConnected}` — that this port's SSE
  transport has no equivalent for.

One thing that does fall out cleanly: upstream's `if(typeof n!=="string") return !0`
means **an absent body falls back**, so a transport that cannot supply one maps
to `None` without inventing a policy.

⛔ Do not implement the status half alone. A server answering a rejected POST
with a valid JSON-RPC error is a real protocol failure; re-dialling it would
convert protocol errors into silent transport churn, which is worse than the
missing fallback.

### How it was actually done

**Where it lives decided the cost.** The registry retry seam
(`registry.rs:3667`) varies the `McpTransportSpec`, and `Http → Sse` is a
lossless four-field conversion, so it looked like the home. It is a trap:
`oauth::server_key` folds the spec KIND into the key, so re-dialling there would
silently repartition the stored OAuth token and the discovery cache. Upstream
keeps its config and swaps only the transport object.

Putting the rescue inside the transport therefore:

* kept `McpError::HttpResponse` untouched — the body is still in scope there, so
  the 7 construction sites and 4 match sites in `platform-api` never moved;
* made `sawAuthChallenge` a local flag instead of cross-crate plumbing, so
  `McpConnectOptions` kept its `Copy` derive;
* confined the whole change to `platforms/common/`.

Ported in full: the four guards, the `min(5000, max(1000, remaining))` budget,
the three-arm error rule, and `postMethodNotAllowed`'s real job — refusing to
start OAuth when the stream GET answers 401 and the original rejection was a
400/404 rather than a 405. That guard is fallback-only, as upstream scopes it.

**One approximation**, recorded at `choose_rescue_error`: upstream's
`sawAuthChallenge` is set by a fetch wrapper and sees a 401/403 on ANY request in
the dial; only the dial's final error is observable here. Since the predicate
admits only 400/404/405, the two agree except when a 401/403 occurred on an
earlier request of the original dial and was then followed by one of those
statuses.

### 🚨 The prerequisite nobody had recorded — `ff8bda8b4`

Implementing MCP-01 surfaced a defect that existed independently of it: **this
port's SSE transport was not a legacy HTTP+SSE client at all.** `connect_sse`
POSTed to the url it opened the stream on and skipped named SSE events, while
upstream keeps `_url` and `_endpoint` apart and learns the second from a named
`endpoint` event, resolved relative to the stream url, required to be
same-origin, with no sending before it arrives.

So a configured `type: "sse"` server following that contract **never received
this port's outbound frames**, and re-dialling as SSE without fixing it would
have connected the stream and posted into the void. The module doc had described
POST-to-the-same-url as a LITERAL contract "matching claude-code's
`SSEClientTransport`" — it did not match it, and this was not a recorded
divergence. `connect_sse` now takes an explicit `SseEndpointMode`; the IDE
variant, which really does serve both directions on one url, keeps its
behaviour. Two e2e mocks were not conformant either and now name a different
path, so those round trips only pass if the client honours what the server
named.

## MCP-02 — a project `.mcp.json` can smuggle an unresolved `${…}` into a command — ✅ FIXED in `7a710a252`

`src_175313539.js` @26576, in the project-scope arm of the approval builder:

```js
let a = {...n, command:u(n.command), url:u(n.url),
         args:n.args?.map((b)=>Te(b,e)), env:ct(n.env,e), headers:ct(n.headers,e)};
if (r === "project") {
  if ([a.url, a.command, ...a.args??[]].some((S)=> S!==void 0 && S.includes("${")))
    return { refused: "Its url, command or args reference an environment variable; on connect Claude Code would expand it into a repo-authored command or url. Add it manually with `claude mcp add` if you trust this repo." };
  …
}
```

Two details carry the whole property. The test runs on the **expanded** value
`a`, not the raw config — so a `${VAR}` that resolved is fine, and only one that
did NOT resolve (unset variable, literal `${…}` surviving) is refused. And it is
gated on `r === "project"`: the same shape from a user-scoped config is allowed,
because the user authored it. The threat is a repo-authored `.mcp.json`.

**Port:** absent. `mcp/src/server_gate.rs` approves/rejects project entries **by
name** (`approved_project_servers` / `rejected_project_servers`) with no
content test. `env_expansion.rs:133`'s `contains("${")` is an early-out inside
the expander, and `discovery_cache.rs:749`'s `has_placeholder` is cache-safety —
neither refuses anything.

**Fixed in `7a710a252`.** `McpServerBlockReason::ProjectUnresolvedEnvRef`,
refused ahead of the approval bookkeeping so `enableAllProjectMcpServers` cannot
admit it either.

⚠️ Worth carrying forward: `load_mcp_servers` decides whether a blocked entry is
disabled by **matching on the block reason**, so a new reason the match does not
list is inert. Removing that arm left the entire 688-test `mcp` suite green —
the gate would have read as implemented and done nothing. Any future block
reason needs the same wiring test.

## MCP-03 — `managedMcpServers` is not read — ✅ FIXED in `807dc6fb4` + `62fa5e3fb`

2.1.267 accepts managed MCP servers under a `managedMcpServers` **settings key**,
with its own shape validation:

> `"managedMcpServers" must be an object keyed by server name (the .mcp.json
> mcpServers shape; Claude Desktop's array form of its same-named key is not
> accepted here: use the server name as the key and "type" instead of
> "transport"). No managed MCP servers are installed from it until it is fixed.`

**It was not a different intake shape — upstream has BOTH channels**, and they
carry different trust. Investigating before acting turned up the opposite of the
worry: the port was *stricter* than upstream, not looser.

### The trust rule — `807dc6fb4`

`apply_enterprise_mcp_policy_with` filtered EVERY `managed-mcp.json` server
through the allowlist, so an organization shipping both a file and an
`allowedMcpServers` list had its own servers dropped unless it listed them.
Upstream exempts them, conditionally (`F6`):

```js
if (Pme(e,n)) return !1;                                   // denylist wins
if (n?.scope !== void 0 && XJ(n.scope) && !n.expandedFromEnv && !n.pluginSource) return !0;
```

with `XJ(e) = ["enterprise","managed"].includes(e)`. The `expandedFromEnv`
conjunct is the whole point: managed settings forbid `${VAR}` outright — *"a
managed settings document must not read the user's environment"* — so a document
that cannot read the environment is fully determined by what the organization
signed off on and is trusted, while one that CAN stays on the allowlist's hook.

`expandedFromEnv` is computed from the RAW document before expansion (claude
`Fxo`, testing exactly the fields that expand), because afterwards the reference
is gone and the entry looks like any other literal. Ordering is pinned: the
denylist runs BEFORE the exemption, so an organization cannot deliver its way
past its own denylist.

`!n.pluginSource` has no port analogue and is vacuous here — `mcp` carries no
plugin-source provenance, and a plugin server is never loaded at
Enterprise/Managed scope.

### The settings key — `62fa5e3fb`

Its contract is much tighter than the file channel's: **http/sse only** (an
organization may push a URL to every user, not a program to run on their
machine), **no `${VAR}`**, name-shape validated, `deniedMcpServers` still
applies, no `allowedMcpServers` entry needed. Precedence follows
`$Tn = [enterprise, managed, local, project, user]`, earlier wins a name.

On `${VAR}` upstream has belt and braces — the schema refuses one AND the load
runs with `expandVars: false`. This port implements the belt, rejecting such an
entry before the parser, which is why it needs no no-expansion parse path.

## MCP-04 — three further new gates, all classified

| gate | oracle | port | disposition |
|---|---|---|---|
| `tengu_mcp_legacy_sse_fallback` | 3 | 0 | MCP-01 above |
| `tengu_sdk_mcp_manifests` | 2 | 0 | SDK control-protocol surface |
| `tengu_sdk_workspace_trust` | 2 | 0 | SDK `initialize: workspaceTrust` |
| `tengu_plugins_sync_mcp_relay_wait` | 2 | 0 | telemetry only (reconcile/connect budgets) |

## CLOSED since the 251 audit

`tengu_surface_failed_mcp_servers` — the 251 report recorded the port defaulting
this flag **false** against an oracle default of **true**, i.e. failed MCP
servers were never surfaced in the ToolSearch note. `tools/meta/src/tool_search.rs`
now reads `telemetry::flag_bool("tengu_surface_failed_mcp_servers", true)`.
Strike it from the backlog.

## Still open from the 251 audit, unchanged

Both remain 0 hits and both are still blocked for the reasons recorded then:
the startup `N MCP servers need authentication` warning (no runtime producer was
ever recovered from the binary — a changelog sentence is not a producer), and
managed-settings MCP startup-mode approval exemptions (owned by the
managed-settings approval subsystem, not this crate).

---

## Recommended order

1. ~~MCP-02~~ — **done** (`7a710a252`).
2. ~~MCP-01~~ — **done** (`ff8bda8b4` + `a1e070f4e`).
3. ~~MCP-03~~ — **done** (`807dc6fb4` + `62fa5e3fb`).

**No open items remain in this audit.**
