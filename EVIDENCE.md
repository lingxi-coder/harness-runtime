# Realtime ownership checkpoint evidence

Status: **HOLD; not a release or App integration.** No source was pushed and no App dependency pin was changed. Current product clients do not yet implement the provenance fields or explicit confirmation UI described in `DESIGN.md`.

## Exact source checkpoints

- SDK isolated worktree `delivery-round1/llm-client`: `4667773f06cc7f70b81dc02ee9f8d25d9d61fce9`, parent `dac38d7973ff4e33bc2ab9d8445404a9f24df793`. The former exposes response attribution; the latter closes controls after misuse of the plain event receiver. Both depend on the held local `3cb37e3` candidate chain; they are not independently ready for the published main line.
- Harness isolated worktree `integration/realtime-ownership/harness-runtime`: `e07d1a2a6780bd49ae2e699231c03144d6e77e54`, parent `8acd3675f8300f652c5b7ba93ee163c04d0c8acb`, based on original Harness pin `e59c7ed9720edd2cbae866c193c8757948aec490`. `8acd3675` requires the SDK attribution API; `e07d1a2a` adds independent host confirmation for unattributed tool calls. Both worktrees were clean after their scoped commits.
- Final Harness file Git blobs: `crates/core/src/host/permission_gate.rs` `dedf1861db3d895fa954adcab59b2083ba256440`; `crates/permission/src/policy_gate.rs` `b634c8b98444dc6671789b41c1e08f0663fd8475`; `crates/client/src/adapter/permission_gate.rs` `58176801668581257f715c8d7fab140bc20c6769`; `crates/orchestrator/src/conversation/drivers/realtime.rs` `f884f048d4a1a63d2109a6b5b8a67d0a407c7d26`; `crates/orchestrator/tests/realtime_agent_test.rs` `9bd70ecbed6d3f47a0e28ecc8ee9d7dbe464d347`.

## Executed tests

SDK used Rust 1.94.0, the existing private Cargo home/target, `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`, `CARGO_INCREMENTAL=0`, and jobs 2:

```sh
cargo test --offline --locked --test realtime --test gemini_live -j 2
```

`integration/realtime-ownership/sdk-tag-v2.log` SHA-256 `41f5fef2405cd2fc1e721b49bbc2779be0289a0298e5b291ae8720add8b0e2b5`: realtime 24/24 and Gemini 13/13 passed.

Harness used the same settings, plus an explicit Cargo path override selecting the SDK checkpoint without changing the Harness manifest or lockfile:

```sh
cargo test --offline --locked --config 'paths = ["/Users/luolingfeng/lingxi/worktrees/media-architecture/validation/gates-fix-20261030/delivery-round1/llm-client"]' -p orchestrator --test realtime_agent_test -j 2
cargo test --offline --locked --config 'paths = ["/Users/luolingfeng/lingxi/worktrees/media-architecture/validation/gates-fix-20261030/delivery-round1/llm-client"]' -p permission --lib unattributed_realtime_confirmation_is_not_policy_bypass -j 2
cargo test --offline --locked --config 'paths = ["/Users/luolingfeng/lingxi/worktrees/media-architecture/validation/gates-fix-20261030/delivery-round1/llm-client"]' -p client --features adapter --lib unattributed_realtime_requires_a_client_confirmation_capability -j 2
```

- `harness-openai-regressions.log`, SHA-256 `231841be0b1f27e88db740df749f565a935c4594e66420de4acfccaa4c0d15ee`: 49/49 passed at the OpenAI checkpoint.
- `harness-confirmation-final.log`, SHA-256 `421d455c212bce5417f3032053e65c8d78e25110cd3c0451f0a35be61d7e6bfc`: 53/53 passed, including both old-frame schedules, attributed tool continuation, wrong/missing/reused identities, failed/cancelled responses, explicit confirmation, default-deny, per-call prompts, and pending-prompt cancellation.
- `policy-confirmation.log`, SHA-256 `382d0d29ef572ba6b7096ddc5137cc6005a8dd9c8ea0fd18fd202f12343ec24a`: 1/1 policy bypass test passed.
- `client-adapter-confirmation.log`, SHA-256 `6cdd43613eaf4911912cb8ddeed3c2baeb3ba75844fccf0e04ef57b8a095d346`: 1/1 actual adapter-feature test passed. A prior no-feature run selected zero tests and is not counted.

The explicit `AllowAuto` denial is visible in the source match but was not separately exercised; its unrun experimental test was removed before the final commit. Tests use deterministic local transports/provider frames and simulated approval gates, not a provider service or real person. No device, packaged App, live network, final CI, or SDK-publication test was run.

The shared existing bridge binary was not rebuilt or replaced; after testing its SHA-256 remained `45ea9849ea143a28af1d283fb8e728537ef59e3194dde737d1cacc6c3ffc26c9`. The last monitored build completed before the stop instruction; no further build was started. Available filesystem space was approximately 6.7 GiB at handoff.

## Remaining blockers

The OpenAI tag binds response/tool frames to the host response request; it does not prove which microphone input was semantically consumed. Gemini does not provide the same correlation. The current ordinary client lacks the explicitly-described confirmation capability, so unattributed tool requests fail closed; App currently maps most Harness failures to a generic message. Unattributed audio/transcripts and stored history still need a real session-level provenance representation and consumers must not infer a turn. Coordinate the single current App protocol, user-visible explanation, history persistence, and clients as specified in `DESIGN.md`; review and test before integrating or publishing.
