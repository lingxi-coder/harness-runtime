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
| Provider OAuth authorization, token exchange/refresh, device flow and account API protocols | SDK |
| Credential storage, browser/callback UI, refresh coordination and account selection | Host |
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

The preceding model-contract migration published SDK revision
`1a60e73ad12e663be768232a91cbd3ea693aca6e`. The development lockfile retains
the local path package through the root patch, so refreshing it does not change
its contents. No real-provider or physical-device acceptance was run.

Publication verification (2026-09-29): an independent source copy without the
local SDK patch fetched the published revision from the canonical Git URL.
`cargo check -p harness-runtime --all-features` passed, and locked Cargo metadata
confirmed the exact SDK Git source. The local minimal-feature check, formatting,
architecture gate and gate self-test also passed.

## Authentication boundary

`lingxi_llm_client::auth::oauth` owns Anthropic, OpenAI and Copilot authentication
protocols. This includes authorization URL encoding, shared PKCE generation,
token exchange and refresh request/response handling, device authorization
responses, and provider account/profile queries. Operations use SDK `Transport`
and bounded `HttpExecutor`; they do not schedule refreshes or replay model calls.
Desktop and mobile inject the same configured transport used for model calls.

Host OAuth adapters retain secure storage, external credential helpers, browser
opening, loopback callbacks and mobile redirect handling. They also retain
single-flight refresh coordination, background task cancellation, token rotation
persistence, account selection and telemetry. A provider rejection of a refresh
credential is distinct from temporary transport/server failure. Neither a token
refresh nor a successful login authorizes retrying a model request with an
unknown execution outcome.

SDK authentication must not depend on platform-api, the host credential manager,
UI callbacks, process environment conventions or runtime background tasks.
General MCP OAuth remains a separate host concern. The architecture gate rejects
provider authentication endpoint implementation outside the SDK alongside model
protocol bypasses.

Authentication migration regression checks (2026-09-29): runtime library and
integration suites passed 1,236 tests; SDK all-feature auth tests passed 47 tests.
Desktop connect tests passed 7 tests.
Coverage includes token wire compatibility, explicit rejection versus temporary
failure, token rotation, single-flight refresh, callback redirects, deadlines,
response-body cancellation and error redaction. Runtime all-feature test
compilation, minimal-feature compilation and iOS/Android arm64 mobile/UniFFI
checks passed. These checks use the editable SDK submodule; the authentication follow-up
has not yet been published or validated with live provider credentials.

The authentication follow-up pins SDK commit `a4a9880fa02737d95665b3215f6743dfc128f4a4`.
Both repositories are committed locally; publish the SDK before the parent.

## Canonical usage, pricing and authentication data

Provider measurements remain SDK `UsageReport` and `InferenceReport` throughout
execution. `ExecutionUsage` adds host presentation and frozen settlement data;
it is not another token model. SDK `Usage.output_tokens` includes reasoning.
Legacy UI/history adapters subtract reasoning only when projecting the old
separate visible-output bucket. Missing or partial reports cannot establish a
complete, zero-cost attempt. Frozen pricing is captured for the physical attempt,
not recomputed from the active account or current configuration at settlement.

The SDK owns interactive price bounds across token bands, dated rules, service
tiers and peak schedules. The host validates the route identity and applies its
budget/currency policy to those bounds. Settings price overrides are an explicit
serialization adapter to SDK pricing, not a provider pricing implementation.

SDK auth also interprets AWS exported credential JSON, Anthropic subscription
identifiers, scope capabilities and quota headers. The host keeps account-scoped
quota state, workspace trust for external credential commands, and product
subscription gates. Provider OAuth config/PKCE/token/profile forwarding modules
and the SigV4 forwarding module have been removed. Copilot's host module now
contains only application selection and editor identity.

`oauth::lifecycle` shares token hashing, refresh preflight, failure classification
and telemetry helpers. Provider drivers retain their different storage slots,
identity metadata and refresh policies. SDK operations do not start background
refresh tasks or retry model execution.

This follow-up pins SDK commit `92b6f24f10abfac02e4d7f9929eaf74c6d084a59`.
Publish the SDK commit before publishing the parent commit. A local path patch
build does not validate a fresh checkout of the published Git dependency.


Validation for the canonical usage/auth-data follow-up (2026-09-29):

- SDK all-feature library tests: 271 passed.
- Runtime library and integration tests: 1,226 passed across 33 targets.
- Desktop/runtime all-feature test compilation and minimal-feature compilation
  passed; iOS and Android arm64 mobile/UniFFI compilation passed.
- Focused pricing tests passed: 16 fixed-price cases and 6 host admission cases.
- Focused downstream checks cover canonical reasoning subsets, cache TTL,
  provider-metadata independence, complete/partial snapshot replacement and
  durable usage round-trips. Formatting and the architecture gate passed.

These are local submodule, mocked-provider and compile checks. Real provider
accounts, physical mobile devices and a fresh published dependency checkout
have not been validated for this follow-up.
