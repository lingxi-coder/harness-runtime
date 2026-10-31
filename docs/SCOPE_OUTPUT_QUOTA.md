# Host-owned generation output quota

`DesktopConfig.scope_output_quota` is an optional in-memory capability for an
isolated invocation. Ordinary Desktop sessions leave it unset. It is not loaded
from settings, request JSON or model input.

The trusted host constructs one `Arc<llm_runtime::ScopeOutputQuota>` with two
positive integers: a per-request output limit `C` and a physical dispatch limit
`N`. Every service and helper for that invocation must retain the same Arc.
For supported providers that honor their encoded limit, the requested generation
output is bounded by `N * C`. This is not an input-token, total-token or monetary
budget. Exact counting endpoints, authentication and attachment uploads are not
generation requests and do not spend this counter.

The main Desktop assembly and `credentials::api_service_from_stack` attach this
capability to their `ApiService`. Service forks preserve it, including App forks.
Compaction, recap, automatic/session memory and vision side queries using the
Session backend therefore share it. An Independent side-query backend or a
direct `ModelRuntime`/provider-services caller is outside this capability and
must not be used for a limited invocation.

Each retry clamps the explicit/default output request before preparation. After
SDK sealing, the service checks both the resolved budget and final wire bytes,
including generation-count multipliers. A missing or excessive limit, or `n`,
`best_of` or candidate count other than one, is rejected. Unsupported subscription
routes fail closed; removing the limit and trying again is not permitted.
No sealed or signed request is edited to repair an unsupported encoding.

At the physical HTTP/WebSocket generation marker, the driver reserves both the
request's existing `ApiAttemptsLeft` and the invocation quota. Admission checks
may run repeatedly without spending. Reservations roll back before dispatch;
once the host marks a request dispatched, neither ambiguous failure nor missing
usage refunds it. The existing operation budget remains independently enforced.
Quota exhaustion is terminal, with no retry or connection failover.

Optional WebSocket prewarm becomes a no-op for limited services. Independent
DeepSeek web requests are refused because they do not use the generation driver.
Normal services without a quota retain their existing behavior.

`revoke()` permanently blocks future generation dispatches, including after an
awaited host admission. It does not prove termination of a request or tool that
was already admitted. A host still needs process isolation, tool grants, an input
bound, wall-clock/turn limits, an owner approval route, a distinct original-human
question route and reliable termination. A restarted host must not recreate a
fresh quota and replay an ambiguously executed invocation. This API alone does
not enable Team Bots or establish their acceptance.

Validation:

```sh
cargo test --offline --locked -p llm-runtime --lib --no-default-features -- --test-threads=1
cargo check --offline --locked -p harness-runtime --lib
```

The quota regressions cover concurrent reservations, pre-dispatch rollback,
explicit output overrides, retries, service forks/auxiliary calls, WebSocket
generation/prewarm, sealed protocol fields, multiple completions, unsupported
subscription routes and revocation during awaited admission. Transport fixtures
do not establish that a real provider honors its limit or that a product host
always selects the limited path.
