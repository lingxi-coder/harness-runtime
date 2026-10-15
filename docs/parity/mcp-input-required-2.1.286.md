# Ordinary MCP input_required in Claude Code 2.1.286

The ordinary modern result driver is implemented in the current production
MCP client, common remote transport, and POSIX stdio transport. It uses the
existing local handler registry and the existing trusted roots/elicitation
context. No older Harness API alias or result-envelope adapter is retained.
The latest native fallback handshake remains part of protocol negotiation.

## Native reference

The reference is the darwin-arm64 2.1.286 binary with SHA-256
`75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
The hash-pinned generator executes extracted SDK codec, schema, driver, client
handler wrapping, and request/progress/timeout methods. Its byte ranges and
hashes are recorded in the saved fixture.

```sh
node scripts/tests/mcp_input_required_286_oracle.mjs /path/to/2.1.286 --check
```

The current native fixture contains **84 primary cases**: 26 codec, 18 schema,
and 40 driver cases. Three additional cases record JavaScript safe-integral
JSON serialization. The fixture separately records exact numeric-result JSON
from `es.decodeResult` and public `Client.request`: the codec preserves
`content`, `structuredContent`, `_meta` order, while public result projection
places `_meta` first. Production implements each boundary explicitly; removing
`resultType` must not accidentally reorder the remaining properties.

The generator controls I/O, handler callbacks, virtual timers, and Node's
WHATWG URL constructor. It does not execute the native CLI or its Bun URL/IDNA
tables. Rust production and real loopback/stdio execution have separate tests.

## Production contracts

- Ordinary auto-fulfillment defaults on, with ten additional rounds: at most
  eleven outgoing requests. Modern remote Tasks remain behind the native
  default-false `tengu_mcp_tasks` feature and are not enabled by this work.
- An `input_required` result requires a nonempty request object or string
  request state. State-only rounds wait 250 ms. Each retry starts from the
  original method and parameters, replaces current-round answers, and uses the
  original request state when the current result omits it.
- Embedded roots, elicitation, and sampling requests invoke registered local
  handlers concurrently. Capability/registration checks precede parameter
  validation. Handler failure cancels siblings and preserves the error; no
  callback RPC is sent to the remote server.
- Native parameter/result projection differs by method. Roots callbacks retain
  common wire parameters, including unknown fields; the independent standalone
  InputRequest schema has stricter validation. Elicitation and sampling use the
  native schema projection. Integer object keys follow JavaScript enumeration.
- Retry calls preserve headers, progress handling, caller cancellation, and
  total-time accounting. A local handler can finish after the total budget;
  native behavior checks that budget before the next request instead of imposing
  a new hard handler deadline.
- Initial catalog loading registers trusted roots/elicitation handlers before
  publishing the client. It uses the same hook and pending-elicitation counter
  as later tool calls.
- Streamable HTTP POST response readers run independently. An open SSE response
  does not block a retry POST; normal success keeps later SSE notifications
  alive. Timeout/drop cancels the assigned request stream, expired queued calls
  cannot POST later, and connection close aborts and drains owned readers.
  No arbitrary concurrency cap was introduced.

Production entrypoints are
[`mcp_result.rs`](../../crates/core/src/host/mcp_result.rs),
[`client.rs`](../../crates/mcp/src/client.rs),
[`registry.rs`](../../crates/mcp/src/registry.rs),
[`mcp_remote.rs`](../../crates/platforms/common/src/mcp_remote.rs),
[`mcp_http.rs`](../../crates/platforms/common/src/mcp_http.rs), and
[`mcp.rs`](../../crates/platforms/posix/src/mcp.rs). The shared JSON-RPC adapter
resides below both clients; platforms do not depend on the MCP engine crate.

## Executed validation and limits

The three new MCP test targets passed **13 tests**, including real connection
byte captures, registered handler projections, initial catalogs, ordinary
stdio calls, progress, and cancellation. The HTTP target passed **six real
loopback tests**, including concurrent retries with open SSE bodies, later
notifications, session-header response ordering, socket cancellation, and
expired queued calls.

The broader MCP/common/POSIX all-target run executed 46 targets. Two old unit
fixtures required the native modern catalog fields and the correct `_meta`
version path. After those fixture-only corrections, both complete library
suites passed again (MCP 661, POSIX 163). Combining those library results with
the unchanged targets from the broad run gives **1,099 passed and one ignored**.
The real tmux integration is explicitly ignored without its required host.

Remaining acceptance boundaries:

- Additional schema cases compare acceptance, error code, and structured data;
  their entire validation-message strings are not all byte-locked.
- Safe integral doubles, including negative zero, have exact serialization
  checks. JavaScript formatting of every scientific-notation threshold and
  unsafe integer is not established by these cases.
- The controlled URL constructor is not proof of the native Bun IDNA table.
- Elicitation `applyDefaults=true`, peer JSON-RPC error frames, and remote Tasks
  scheduling/reconnect/sidecar lifecycle remain outside this native oracle.
- These tests do not establish a full native CLI MCP session or a live provider,
  physical device, or packaged host acceptance.

The complete latest-Claude-Code alignment goal remains open.
