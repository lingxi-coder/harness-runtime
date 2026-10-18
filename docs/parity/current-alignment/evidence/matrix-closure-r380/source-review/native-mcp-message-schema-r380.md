# Native MCP JSON-RPC message schema (2.1.295)

Source-only mapping for the MCP-owned frame decoder and bounded transport
review. No Native process, capture, Cargo command, or production-source edit
was used.

Frozen Native binary SHA-256:
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.

The schema source range is recorded in
`acceptance/source-alignment-295-r31/native-policy-source/manifest.json` at
binary offset `222984265`, length `18579`, SHA-256
`8fae67acf6335fff60e0567e103cfd8b757a47808abf4d2693ca50e1678bf123`. I
verified that hash by slicing the frozen binary directly. The SDK transport
source range is recorded in the same manifest at offset `223010932`, length
`317049`, SHA-256
`c65d73f55bdb2dbb76f39d4b8a94d3679f2b734aa2e7b03c9fbee72b2174f174`; it
imports the exported `aX` schema and calls `aX.parse(JSON.parse(...))` for
stdio, SSE, and streamable-HTTP messages. The schema, `.int()`, SDK transport,
and both stdio-reader source-range hashes in this note were directly verified
against the frozen binary.

`aX` is the union of four schemas:

| Variant | Required contract |
| --- | --- |
| Request | Strict envelope: `jsonrpc` is literal `"2.0"`; `id` is string or safe integer; `method` is string; `params` is optional and, when present, is a loose object with optional `_meta`. |
| Notification | Strict envelope: `jsonrpc` is literal `"2.0"`; `method` is string; `params` has the same optional loose-object shape; `id` is absent. |
| Result response | Strict envelope: `jsonrpc` is literal `"2.0"`; `id` is string or safe integer; `result` is required and is an object schema with optional `_meta`, not an arbitrary JSON value. |
| Error response | Strict envelope: `jsonrpc` is literal `"2.0"`; `id` is optional string or safe integer; `error` is a required object with integer `code`, string `message`, and optional unknown `data`. The nested default object accepts and strips unknown keys. |

`aX` is defined as `Be([ogn,sgn,C1t,A1t])`. `egn=Be([o(),v().int()])` and
`ngn=_t({progressToken:egn.optional(), ...})`, so `progressToken` is a string
or finite safe integer. The `.int()` implementation's range is recorded in
the same manifest at offset `186578912`, length `90493`,
SHA-256
`8886b2923ae669a979e16ab15d7a8151571d85efcd7471e0ba226d4e2d53559f`); its
number check applies `Number.isInteger` and `Number.isSafeInteger`. The current
H request-meta validator uses this same string-or-safe-integer domain.

An envelope with `result:null`, `error:null`, or neither result nor error
matches no response variant. `error.data:null` remains valid because `data`
uses the unknown-value schema. Strict envelope objects reject extra top-level
keys. Request/notification params and result metadata use loose-object
schemas; `_meta` itself must be an object when present. The optional result
`_meta.serverInfo` property has a `catch(undefined)` schema, so an invalid
value is caught rather than making the whole result invalid. The source strips
unknown keys from nested default objects such as `error` and valid
`serverInfo`. The current H adapter validates the schema, applies schema-key
normalization and loose-object passthrough, and retains source projections for
nested values. Its `jsonrpc_method` maps the original UTF-16 code units into
`JsonRpcString`, preserving method identity for the frame/public-message path.

For parser input semantics, `Utf16JsonProjection::parse` is the closest
existing H carrier: `crates/json-projection/src/lib.rs` documents JavaScript
binary64 number parsing and preserves UTF-16 strings; its duplicate-key test
(`duplicate_exact_keys_keep_the_last_value_and_original_order`) confirms the
last duplicate value wins, as after `JSON.parse`. The MCP adapter validates
this projection before converting it to generic `jsonrpc::Message`, whose
numeric fields are narrower than the Native safe-integer domain. Generic
JSON-RPC decoding remains protocol-agnostic and should continue accepting
`result:null`; MCP-specific schema enforcement belongs in this adapter.

## Native error and connection behavior

| Transport path | Source-observed parse failure behavior | What is not established by these ranges |
| --- | --- | --- |
| stdio | `readMessage` consumes a line before `JSON.parse` / `aX.parse`; its local catch continues only for `SyntaxError` and rethrows schema errors. The `processReadBuffer` caller catches any thrown error, invokes `onerror`, and continues its loop. | Final reader/child cleanup and effects of a custom `onerror` handler. |
| SSE | The event handler catches the parse error, calls `onerror`, and returns without explicitly closing the event source. | Whether a pending request later times out or is settled by a higher client layer. |
| Streamable HTTP | JSON response parsing catches the error, calls `onerror`, and rethrows from `_send`; no explicit close occurs in that catch. | Wrapper-specific retry or outer caller reporting. |

The stdio ranges are `src_205677138.js` (offset `205677138`, length `1094`,
SHA-256
`49cab98a2a071df2dfa96e471ea81c2c96ee8555c98e77d88ed40e1ba2f64d92`) and
`src_205678233.js` (offset `205678233`, length `1618`, SHA-256
`02d465c5e68b18dfd4caf7711446e42428011d76c5345626a5d9b1e3e2dde9a2`). The
outer caller consumes and continues after either malformed JSON or schema
errors; the inner `readMessage` rethrow alone does not establish process
termination. H's line decoder's consume-and-skip policy matches that bounded
continued-read behavior.

## Inbound unknown request methods

In the `src_223010932.js` slice, `MCPClient._onrequest` has two branches that
respond with code `MethodNotFound` and the fixed text `Method not found`: a
method unavailable in the negotiated codec (local offset `87059`, absolute
offset `223097991`) and an unregistered request with no fallback handler
(local offset `87192`, absolute offset `223098124`). The request ID remains the
input ID. The generic H JSON-RPC dispatcher preserves a method-bearing default
for generic users; current MCP connection constructors opt into this fixed
message while generic JSON-RPC/LSP keeps its default. The stable
`runtime/r380-jsonrpc-lib-full-final.receipt.json` covers dispatcher policy
behavior (77/77). Current MCP source wiring and the scoped schema, input-required,
null/presence, frame, and production selectors are recorded in the 42-row
checklist. These are source/candidate results, not Native public auth-byte
captures.
