# Model execution and host policy

`lingxi-llm-client` owns the model contract. `llm-runtime` composes it with host
routing, credentials, history, retry policy, telemetry and durable accounting.
The runtime does not expose another provider codec or stream decoder interface.

## Rust API

`LlmRequest` is a host execution envelope, not a second model request:

```rust
let mut call = llm_runtime::LlmRequest::new("model").with_profile("profile");
call.input.max_tokens = Some(1024);
call.execution.query_source = Some("side_query".into());
```

Its fields are `input` (SDK `ChatRequest`), `profile`, `stream`, and `execution`.
There is no `Deref` or compatibility field forwarding. `ExecutionContext` is
skipped by serde, so JSON cannot manufacture admission authority, account scope,
recovery state or exact-string overrides.

`ModelRuntime` replaces `DefaultLlmClient` as the host composition object. Its
low-level execution methods return SDK `ChatResponse` / `ModelStream` directly.
Responses connection state is SDK `ResponsesSession`; no runtime forwarding
session exists. SDK clients are reused across models in the same effective
provider configuration and transport. Changed configuration or transport creates
another snapshot; existing clients and prepared calls retain the previous one.
Credentials are loaded for each request, never stored in the client cache.

`ProtocolFamily` and `AuthStrategy` are the actual SDK types. Reasoning capability
lookup, strict-schema conversion, provider credential application, replay-safety
classification and stream assembly also live in the SDK. The host supplies
selected options and determines what to do when they cannot be satisfied.

## Durable history and UI

`HistoryResponse`, `HistoryEvent`, `HistoryContentDelta`, `HistoryMessageDelta`
and `HistoryStopDetails` are host history/presentation contracts. Their serialized
shapes remain unchanged. These are deliberately separate from SDK model events;
mobile bindings continue to project the existing DTOs at the application edge.

`convert::history_input` adapts durable history once into SDK messages. Historical
replay companions are consumed here; protocol compatibility is evaluated by SDK
replay policy. Exact JavaScript UTF-16 strings remain a host sidecar. Cache
positions and exact-string paths are remapped together whenever history blocks
are removed, including thinking-signature recovery.

`ExecutionContext.input_protocol` records the family used for canonical history.
When fallback or connection failover selects another family, `ModelRuntime`
adapts a clone with SDK `replay::adapt_request` before preparation. The explicit
lossy policy removes incompatible cache policy and replay metadata, and returns
block positions for exact-string remapping. The source request is preserved for
later attempts; scoped continuations cannot move across protocol families. The
SDK adaptation itself does not authorize or perform a retry.

The reverse history projection preserves existing companion records where the
persisted history format cannot represent SDK tool identity or signatures in its
visible block. Those records are a storage contract, not an internal execution
format. Provider argument, signature and native-block assembly use the SDK's
`StreamAccumulator`. Raw observations never become executable tools.

## Provider responsibilities

| Responsibility | Owner |
| --- | --- |
| Codec selection rules, request encoding, header/body merging, signatures | SDK |
| HTTP, SSE, Responses/Realtime WebSocket transport | SDK |
| Reasoning capabilities and provider replay compatibility | SDK |
| Canonical stream aggregation, complete blocks and partial results | SDK |
| Files, audio, images, embeddings, model/provider resources | SDK |
| Credentials, OAuth login/refresh and account selection | Host |
| History normalization, tool pairing and UI/FFI projection | Host |
| Permission, admission, cancellation/deadline and retry decisions | Host |
| Frozen-price accounting, conservative unknown settlement and telemetry | Host |

The physical attempt order remains preparation, admission, one dispatch and
settlement. The SDK's replay-safety classification constrains host retries; it
does not authorize resending. Scoped or explicit response continuations, hosted
execution and unknown native state remain conservative. Missing usage is not a
zero-cost completion.

## Regression boundaries

The architecture gate (`scripts/check-llm-boundary.sh`) rejects production model
endpoints, provider stream parsers, removed client/session wrappers and duplicate
codec/decoder traits. Private codec fixtures are an explicit test-only exception;
their enclosing modules compile only under `cfg(test)`, not the public
`test-support` feature.

Regression suites cover canonical request serialization, account isolation,
configuration/transport snapshots, exact UTF-16, cache remapping, SDK stream
assembly, delayed tool identity, completed blocks before an interrupted stream,
search citations and usage, refusal metadata, continuation and settlement.
Live provider credentials and physical mobile devices are separate acceptance
checks; local mocked transports and compile checks do not establish those results.

## Validation of this migration

- Runtime: `cargo test -p llm-runtime --lib --tests --offline --no-fail-fast`
  — 1,250 tests passed. Loopback OAuth tests were run outside the restricted
  network sandbox.
- SDK: all-feature library plus `model_stream`, `gemini_wire`, `prepared_calls`,
  `responses_session`, `responses_transport`, and `http_transport`
  — 344 tests passed. Documentation tests: 40 passed, 8 ignored.
- Sidequery, compaction and Fusion library suites: 560 passed. Orchestrator
  provider-adapter tests: 23 passed. JSONL resume / exact UTF-16 test: 1 passed.
- Workspace: `cargo check --workspace --all-features --tests --offline` passed;
  `llm-runtime --no-default-features` passed.
- Mobile: `harness-runtime --no-default-features --features mobile,uniffi`
  checked for `aarch64-apple-ios` and `aarch64-linux-android`. Android used NDK 29,
  API 29 compiler tools and an explicit NDK sysroot for bindgen.
- Both Rust formatting checks, diff whitespace checks and the architecture gate
  passed. Existing unrelated documentation/unused-code compiler warnings remain.

Follow-up route-adaptation regression checks passed: 1,052 runtime library tests,
6 connection-failover tests and 8 SDK replay tests. Sidequery and orchestrator
all-feature compilation also passed after the fix.

The SDK is published as `1a60e73ad12e663be768232a91cbd3ea693aca6e`; the root
Git dependency and submodule use that revision. The development lockfile retains
the local path package through the root patch, so refreshing it does not change
its contents. No real-provider or physical-device acceptance was run.

Publication verification (2026-09-29): an independent source copy without the
local SDK patch fetched the published revision from the canonical Git URL.
`cargo check -p harness-runtime --all-features` passed, and locked Cargo metadata
confirmed the exact SDK Git source. The local minimal-feature check, formatting,
architecture gate and gate self-test also passed.
