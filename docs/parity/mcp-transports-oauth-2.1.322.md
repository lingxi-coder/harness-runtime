# MCP SDK, HTTP/SSE and OAuth — Claude Code 2.1.322

## Reference and scope

The public npm `latest` release was checked on 2026-11-09 (America/Los_Angeles)
and resolved to `@anthropic-ai/claude-code@2.1.322`. The installed macOS ARM64
executable reports that version and has SHA-256:

```text
a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450
```

The previous MCP/hooks phase verified the official platform archive's npm
SHA-512 integrity and matched its executable to this installed file. This phase
rechecks the executable hash and fresh release metadata; its receipt retains
the original archive verification. See the parent workspace's
`artifacts/mcp-transports-oauth-20261109/outputs/reference.json`.

This follows [the tool/hook alignment](mcp-hooks-2.1.322.md). Tests distinguish
whole Claude Code execution with local peers from isolated execution of
hash-pinned native JavaScript helpers. Neither implies complete protocol or
real account acceptance.

## Provider independence

LingXi supports multiple LLM providers. MCP transport and OAuth behavior belongs
to the MCP connection, independently of the selected model, LLM protocol,
provider credentials or account. SDK-specific handshake settings do not change
ordinary MCP client capabilities. Claude-compatible control-envelope behavior
does not restrict internal MCP registry/tool invocation to Claude models.

`crates/orchestrator/tests/mcp_multi_provider_322.rs` drives actual MCP discovery,
provider tool-call decoding, tool execution and the next encoded provider
request through Anthropic Messages, OpenAI Chat, OpenAI Responses and Gemini
GenerateContent codecs. It uses a provider-neutral model identifier, fixture
LLM responses and an in-process MCP peer. It checks exact Unicode arguments,
structured-result JSON text, result pairing and ordinary MCP capabilities.
These are local codec integration checks, without calls to provider services.
The existing local ToolSearch gate also remains available on supported
non-Anthropic routes.

## SDK transport contracts

Native loopback probes cover empty SDK initialize capabilities, request ids,
the initialized notification, host control acknowledgements and peer requests
while a tool call is pending. The concurrency probe holds the tool response
until the host observes a response to an inbound MCP ping, exposing a serial
writer deadlock. Incoming ping ids include unpaired UTF-16 source units.

SDK connections advertise `{}` capabilities, start request ids at zero and
bind progress tokens to the router's reserved numeric request id. The SDK
boundary emits the observed native JSON-RPC property order and omits empty
`tools/list` and initialized-notification params. Generic JSON-RPC defaults and
ordinary MCP capabilities remain unchanged.

Host control replies wait independently after frames are admitted in writer
order. MCP requests retain the caller's deadline; the control acknowledgement
deadline applies to notifications/responses. Close/cancellation cleans up
outstanding control requests and progress forwarding. Incoming `mcp_message`
envelopes receive a success acknowledgement; only valid messages targeting a
connected SDK server are delivered.

The SDK-only client installs ping, while roots and elicitation remain
unsupported, as observed in native execution. The registry's second client
wrapper also uses that constructor, so reconnect/wrapping cannot reinstall
ordinary host capability handlers. Manifest replay/parser checks include
unpaired UTF-16 metadata strings and preserve source units.

## HTTP/SSE contracts

Native loopback probes cover Streamable HTTP JSON responses, named-event/CRLF
SSE filtering, JSON response arrays, invalid response media types, authored
headers, HTTP-to-legacy fallback and direct legacy SSE. Assertions use the
recorded request/response bytes, including negotiated session/protocol fields.
GET, POST and explicit disconnect retain their separate header contracts.

Streamable HTTP validates response MIME essence, including the shipped
parser's malformed-parameter fallback. Legacy EventSource retains its separate
case-sensitive prefix guard. Both filter non-message SSE events. JSON arrays
are validated as a whole before messages are forwarded, preserving exact
string source units. Notification/response bodies are drained without routing
them as answers to unrelated calls. Native Streamable HTTP GET accepts a
successful SSE-formatted body without a MIME guard; a live plain-type case
verifies notification delivery and catalog refresh.

Headers follow exact-case object replacement followed by case-insensitive
joining, including authored duplicate names. Required Accept values and POST
JSON Content-Type are enforced; initialize omits session ids; negotiated
protocol headers reach both HTTP and legacy SSE POST. Body-derived method/name
headers apply only to requests. Explicit disconnect uses common DELETE headers;
native's ordinary CLI shutdown did not emit DELETE in these probes, so this is
helper/explicit-cleanup parity rather than shutdown-lifecycle equivalence.

## OAuth contracts

OAuth fixtures use hash-pinned native helpers and disposable local HTTP peers.
They cover authorization URL/form encoding and callback behavior without
opening a real account sign-in or accessing the installed CLI's credential
store. A callback with the wrong state must return an error while keeping the
valid pending flow alive.

LingXi retains its own client identity and provider-neutral MCP authentication.
It does not adopt an Anthropic-hosted client metadata document as a global
identity. This branding/account boundary is an intentional difference from
Claude Code, not an LLM-provider admission rule.

Authorization URLs use native form encoding and query replacement/order rather
than string concatenation. Code exchange, refresh, registration and revocation
fixtures check form/JSON bytes and client-auth selection. DCR preserves the
different property position of an existing curated scope versus a scope
appended by the SDK. CSRF state uses 32 random bytes with base64url encoding.

Discovery implements PRM path-to-origin fallback and the observed OAuth/OIDC
candidate order and protocol headers. The auth-flow adapter uses default
`/authorize`, `/token` and `/register` endpoints after absent metadata, while
preserving genuine HTTP/parse failures; the public discovery-only helper keeps
its error contract. Scope selection covers resource metadata, offline access
and configured-scope precedence.

Callback parsing preserves the native duplicate-key distinctions: duplicate
state is rejected, duplicate code/error values join with commas, and issuer
validation uses the first value. Wrong-state requests receive 400 without
terminating a valid flow. Partial or stalled sockets do not block another
callback. Local tests exercise issuer validation, provider errors and listener
cancellation.

## Verification

| Gate | Accepted result |
| --- | --- |
| JSON-RPC library | 78 passed |
| MCP library, including SDK client and OAuth flows | 749 passed |
| HTTP/SSE source fixtures / loopback integrations | 3 / 20 passed |
| MCP mock, OAuth integration and stored-key identity | 60 passed |
| Real provider-codec MCP roundtrips | 4 passed |
| Desktop SDK module / control gates | 11 / 2 passed |
| Desktop production compile | Passed; 44 warnings retained |
| Installed native CLI | SDK scenario and all 10 HTTP/SSE cases passed |
| SDK helper replay | 38 cases plus concurrent-send/close checks passed |
| HTTP/SSE helper replay | 23 MIME, 8 headers, 2 body-header, 4 JSON, 2 timeout and DELETE checks passed |
| OAuth helper replay | 119 cases plus state fixture passed on Node and Bun |
| Native-source OAuth with real local sockets | Both metadata/default-endpoint flows passed; 23 HTTP requests |

The provider fixture handles Messages token-count requests separately and
provides an explicit application output budget for its neutral model id.
Initial fixture failures did not require a product/provider admission change.
The raw SDK fixture consumer uses the outbound JSON-RPC deserializer; applying
the inbound normalizer there would incorrectly reorder native params/meta.
All exact byte assertions remain in the accepted tests.

The older HTTP timeout regression expected an unresolved call even though HEAD
already rejected it. It now checks the existing local error representation,
next-call recovery and a body that outlives the response-header timeout. Pinned
native replay confirms timeout rejection and timer clearing at headers. Rust's
local `-32001` error text is an existing host mapping, rather than a native
network response. No timeout production behavior was changed for that test.

Reproduce from this repository, using the reference executable:

```sh
node scripts/tests/mcp_sdk_322_oracle.mjs /absolute/path/to/claude
node scripts/tests/mcp_remote_322_oracle.mjs /absolute/path/to/claude
node scripts/tests/mcp_oauth_322_oracle.mjs /absolute/path/to/claude
bun scripts/tests/mcp_oauth_322_oracle.mjs /absolute/path/to/claude
python3 scripts/tests/mcp_sdk_322_live.py --binary /absolute/path/to/claude --output /absolute/new/sdk
python3 scripts/tests/mcp_remote_322_live.py /absolute/path/to/claude --output /absolute/new/remote
python3 scripts/tests/mcp_oauth_322_live.py --binary /absolute/path/to/claude --output /absolute/new/oauth --scenario metadata
python3 scripts/tests/mcp_oauth_322_live.py --binary /absolute/path/to/claude --output /absolute/new/oauth-defaults --scenario missing-metadata
cargo test --locked -p jsonrpc -p mcp --lib -- --test-threads=1
cargo test --locked -p platform-common --lib remote_322_oracle_tests
cargo test --locked -p platform-common --test mcp_http_test --test mcp_sse_test --test mcp_http_lifecycle_test --test mcp_remote_e2e_test --test mcp_remote_322_test -- --test-threads=1
cargo test --locked -p mcp --test mock_mcp --test oauth_flow_test --test server_key_pin_test -- --test-threads=1
cargo test --locked -p orchestrator --test mcp_multi_provider_322 -- --test-threads=1
cargo test --locked -p harness-runtime --features desktop --lib headless::sdk_mcp_transport::tests -- --test-threads=1
cargo test --locked -p harness-runtime --features desktop --lib headless::run::control::tests::native_322 -- --test-threads=1
cargo test --locked -p harness-runtime --features desktop --lib headless::run::control::tests::sdk_message_delivery_requires_connected_server_and_unknown_names_are_ignored
cargo check --locked -p harness-runtime --features desktop --lib
```

The accepted Rust commands use debug info disabled, incremental compilation
disabled, two build jobs and `RUST_MIN_STACK=16777216`; the task's JSON receipts
record the exact commands and logs. Local evidence is under the parent
workspace's `artifacts/mcp-transports-oauth-20261109/`. Its final acceptance,
patch and reference receipts are in `outputs/`, with native raw traffic,
controlled replay receipts and Rust gate logs in `work/`.

## Evidence limits

The [gap-closure follow-up](mcp-gap-closure-2.1.322.md) records the later
implementation and verification of these remaining contracts.

These are finite transport/auth contracts. SDK classifier/admission fields
`connectorId` and `autoModeVerdictTools`, the specialized `mcp_call` diagnostic,
and global initialize-response ordering remain outside this pass. Internal
MCP tool execution remains available independently of the model provider.

HTTP assertions cover field values, framing and source-preserved JSON rather
than physical header ordering/casing, HTTP-library defaults or TLS/proxy
behavior. LingXi retains its branded clientInfo and User-Agent. Per-request
interruption, resumption and long reconnect runs are not established here.

OAuth does not reconstruct every SDK schema constraint, saved-registration
binding/cache lifecycle or account XAA/CIMD path. A valid-state callback with
neither code nor error leaves the native socket unanswered; the candidate
closes that individual socket while retaining the flow. Branded callback HTML
and host-specific error prose are not covered by a global byte-equivalence
claim. The lane summaries retain the complete residual lists.

REA's retained static graph covers selected immutable source excerpts. Its
dynamic and obfuscated references remain partial or unknown; absence from that
graph is not evidence of absence from Claude Code. Source replay uses explicit
stubbed dependencies and is reported separately from complete CLI execution.
One exploratory remote excerpt was not parsed by REA; an incomplete OAuth
redirect-method prefix is retained as text. These exclusions are recorded in
the source receipts rather than treated as complete static coverage.
No real MCP service, account authorization, model service, packaged desktop
app or physical device acceptance is implied.
