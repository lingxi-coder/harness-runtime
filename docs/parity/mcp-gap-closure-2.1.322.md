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
- SDK and ordinary clients with the same authored name retain separate routing
  slots. Public status and direct calls prefer the ordinary connection; SDK
  peer delivery uses the SDK client's own connected state. Model tool
  publication follows the actual namespace update, including clearing it on
  SDK removal. SDK discovery waits for the initialized notification's outer
  acknowledgment without blocking unrelated replies.
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
- CLI: its local host explicitly admits staged input/output under the current
  resolved session directory. Virtual and synced hosts supply their own file
  store. Local CLI startup configuration remains distinct from remote operator
  restrictions and later SDK control registrations.

MCP execution remains independent of the selected LLM provider. The regression
matrix exercises discovery, actual provider codecs, tool execution and result
reinsertion for Anthropic Messages, OpenAI Chat, OpenAI Responses, Gemini,
Bedrock Claude, Vertex Claude, Foundry Claude, Azure OpenAI, Gemini Interactions
and Vertex Gemini, using a neutral model identifier and fake responses.

## Verification

The 21 consolidated Rust gates passed. After the final SDK ownership and ACK
changes, 17 affected gates were rerun; four unchanged implementation groups
retain their verified integration receipts. No empty test selection is counted
as acceptance. The code revision is
`2e2e1c9123b0763c77eea2437d753f72a4d35cc3`, which includes runtime main
`4a98e8c27670b12e386e1ff495d82759ec0f8e2e`.

| Check | Result |
| --- | --- |
| 14 immutable source oracles, Node and Bun | 28 runs passed; 1,769 primary cases per runtime |
| JSON-RPC / MCP / MCP wire libraries | 79 / 800 / 10 passed |
| MCP, OAuth, XAA and SDK policy integration | 73 passed |
| Hooks / plugins / tool API / worktrees | 881 / 237 / 316 / 71 passed; one existing ignored hook test |
| Agent library | 763 passed |
| Platform library / legacy SSE | 138 / 5 passed |
| Remote transport / TLS and proxy | 13 tests / 18 network cases passed |
| MCP tool / HTTP client libraries | 171 / 34 passed |
| Connector aliases / direct-control permissions | 3 / 2 passed |
| Orchestrator MCP / worktree / revocation / tool search | 49 / 4 / 1 / 1 passed |
| Runtime SDK / staging admission / native controls / delivery | 24 / 1 / 2 / 1 passed |
| Provider round trips | All 10 protocol families passed |
| Desktop production library | Offline locked check passed |
| Local Apps gateway and lifecycle | 12 passed; one optional Rust-store/Agent bridge check skipped |
| Node host types | Passed |
| Product CLI | Exact-pin offline build and real-host/production-runner integration passed |

The product branch consumes the exact code revision above; all 87 resolved
package records from the runtime repository use that one Git source. The CLI
consumer test runs the actual `CliHeadlessServices`, real stdio MCP peer and
public production headless runner with the existing explicit isolated
credential store. It verifies binary input/output, output etags and lengths,
stale CAS, traversal and symlink refusal, an unchanged outside canary,
startup-server ownership, request IDs `0/1/2/3/4`, temporary-file cleanup, and
completed shutdown/output delivery. Both directions of control and peer
traffic are retained before parsing.

The actual native CLI probes verify separate SDK/client registration, real
handshakes, tool publication, ordinary-first selection, and pending-versus-
connected SDK peer delivery. They capture both directions of the control
stream. The SDK collision fixture compares 18 native control envelopes and
the ordinary HTTP request sequence. An earlier probe omitted the initialized
notification ACK; its pending-state outputs are retained as exploratory
evidence. The final probes acknowledge every outer notification and wait for
actual tool publication. A non-strict discovery diagnostic is excluded from
acceptance.

Artifacts: `artifacts/mcp-gap-closure-322-20261109/` under the parent workspace.
`outputs/gap-ledger.json` records all 20 accepted contract groups.
`outputs/acceptance.json` binds their acceptance to the tested code revisions,
verified logs, raw receipts and explicit boundaries. `outputs/delivery.json`
records local commits and portable patches; no remote publication was performed.
`work/` retains raw native traffic, source ranges and hashes, source replay
receipts, and candidate gate logs.

Final Rust gates use a task-owned target directory. Immutable dependency
artifacts were seeded with an APFS clone, then all 80 local workspace packages
were removed from that clone before rebuilding. Other chats' target directories
and source checkouts were preserved. After the gates, generated workspace
artifacts in this task's target were cleaned with its build lock closed; raw
receipts and source status were preserved. The CLI uses a separate cache seeded
only with reusable dependency artifacts.

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
device acceptance is claimed by these controlled protocol probes. The built
unsigned CLI correctly refuses startup without its signed credential broker;
the consumer integration uses the established test credential seam and adds no
production broker bypass. The standalone broker guard and the tested host
integration are separate acceptance results.
