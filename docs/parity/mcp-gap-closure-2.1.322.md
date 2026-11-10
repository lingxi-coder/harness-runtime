# MCP, SDK and OAuth gap closure: Claude Code 2.1.322

Reference executable SHA-256:
`a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450`.
The public package metadata was checked against the
[official latest package](https://registry.npmjs.org/@anthropic-ai/claude-code/latest).
This follows the finite contracts and remaining issues recorded in
[the hook pass](mcp-hooks-2.1.322.md) and
[the transport/OAuth pass](mcp-transports-oauth-2.1.322.md).

## Implementation

- SDK controls: real direct MCP execution, checked permissions, staged files,
  deadlines, cancellation, dynamic server lifecycle and OAuth handoff. Control
  envelopes, IDs, progress metadata and initialize ordering use captured native
  contracts. Ordinary MCP transports opt into initial request ID zero without
  changing the generic JSON-RPC default.
- SDK policy: connector aliases are restrictive aliases for deny/ask/hooks,
  never allow aliases. Trusted admission and server-cleared verdict metadata
  require explicit host authority and exact input binding. A local classifier
  approval cannot manufacture server authority.
- Names: exact authored UTF-16 identity survives config, scope merging, agent
  JSON, plugin prefixes, registry lookup, SDK controls and hook provenance.
  Private routing handles are distinct from public identity. Discovered tool
  names follow the native sanitizer; direct calls retain their authored source.
  Non-scalar credential identities use separate secure-storage namespaces so
  they cannot collide with scalar names or affect frozen scalar keys.
- HTTP/SSE: exact malformed-message diagnostics, uncertain-result warnings,
  concurrent-call failure fanout, per-request abort and peer cancellation,
  replay cursors, reconnect timing, silent streams, close/DELETE distinctions,
  redirects and bounded body/event readers. JSON overflow leaves the MCP session
  usable; fatal SSE/syntax failures close it.
- TLS/proxy: native MCP trust and proxy policy has its own client configuration.
  Provider HTTP retains its existing TLS behavior and process environment.
- OAuth/XAA: actual bundled schema validation; registration, secret, issuer and
  redirect binding; discovery cache; CIMD/DCR; refresh leases; callback and manual
  input lifecycle; client authentication, revocation, JWT and IdP state.
- Hooks: actual auto-classifier denial and exact provenance; WorktreeCreate
  selects and validates the folder before creation, with hook-owned removal,
  resume and personal-origin restrictions. Optional scratchpad base fields use
  an explicit host-supplied path.
- Local Apps: the gateway sends a real tools-list change on successful SSE
  attachment, preserving discovery after changes made while disconnected.

MCP execution remains independent of the selected LLM provider. The regression
matrix exercises discovery, actual provider codecs, tool execution and result
reinsertion for Anthropic Messages, OpenAI Chat, OpenAI Responses, Gemini,
Bedrock Claude, Vertex Claude, Foundry Claude, Azure OpenAI, Gemini Interactions
and Vertex Gemini, using a neutral model identifier and fake responses.

## Verification

Final consolidated gates are in progress. The machine-readable gap ledger and
acceptance receipt under the artifact directory are the authority for pass
counts and the exact source snapshot.

Artifacts: `artifacts/mcp-gap-closure-322-20261109/` under the parent workspace.
`outputs/gap-ledger.json` lists the contracts; `work/` retains raw native traffic,
source ranges and hashes, source replay receipts, and candidate gate logs.

Final Rust gates use a task-owned target directory. Immutable dependency
artifacts were seeded with an APFS clone, then all 80 local workspace packages
were removed from that clone before rebuilding. Other chats' target directories
and source checkouts were preserved.

Complete native OAuth runs use a verified macOS sandbox that denies host
credential files, all Mach lookups, and the system security/browser commands.
Fake-file and executable canaries prove the denial before each CLI login.
Pasted redirect, actual callback with 1,001 preceding query keys, cancellation,
and SDK URL/callback handoff have separate receipts. All authorization servers,
tokens, certificates, MCP peers and model responses in these probes are fake.

## Host identity and acceptance boundaries

LingXi keeps its own clientInfo, User-Agent, callback branding and OAuth client
identity. Authored JSON, form, SSE and diagnostic bytes are tested separately
from physical HTTP header order/casing and cryptographic TLS bytes.

The verified public native initialize response and this host advertise the same
three supported control capabilities. They do not advertise `host_worded_asks`;
internal `permissionDisplay` output cannot authorize a call through an unsupported
card path. Scratchpad-disabled hosts omit its field and allocate no fake folder.
Server-classifier verdict stamping remains conditional on real host-provided
server authority; it does not change local classifier decisions or provider
routing.

Finite helper replays and static REA graphs are recorded separately from full
CLI execution. No real MCP/IdP/model account, signed app package or physical
device acceptance is claimed by these controlled protocol probes.
