# Refusal API-text snapshot: current Host sources and wiring map

Checked against the current shared Harness tree on 2026-10-04. This is a
read-only source map; it changes no runtime behavior. The Native facts and
cold-state distinctions are documented in
[the 2.1.289 formatter-input audit](mods-refusal-formatter-inputs-2.1.289.md)
and [the cold-state audit](mods-refusal-cold-state-2.1.289.md).

## Current state

`RefusalApiTextSnapshot` has **10 non-brand fields**: `serving_model`,
`model_eligible`, `display_label`, `model_family`, `fable_copy_suppressed`,
`opus_5_5_exception`, `help_url`, `provider`, `interactive`, and
`feedback_eligible`. Brand copy is a separate three-string value. The struct is
defined in [`core/src/host/refusal_api_text.rs`](../../crates/core/src/host/refusal_api_text.rs).

The consumer path exists, but production composition does not attach its source.
`ProviderApiAdapter::new` initializes `RefusalApiTextSnapshotSource` as `None`;
desktop and mobile both construct the adapter without calling
`with_refusal_api_text_snapshot_source`. The only current calls to that builder
are adapter tests. The `ConversationApi` and `SubagentApiClient` default methods
also return `Ok(None)`. When a refusal fallback is declined in the Agent stream,
the runner asks for a snapshot using the current serving model and profile; if
the source is absent, it logs that fact and does not invent a text row.
See [`provider_adapter.rs`](../../crates/orchestrator/src/provider_adapter.rs),
[`conversation.rs`](../../crates/orchestrator/src/conversation.rs),
[`agent/api.rs`](../../crates/agent/src/api.rs), and
[`agent/runner.rs`](../../crates/agent/src/runner.rs).

There is one API-shape issue before incomplete data can be represented cleanly:
the optional adapter source is currently
`Fn(model, profile) -> Result<Snapshot, LlmError>`. Once attached, it cannot
return “route resolved, but authoritative Native policy facts are still
unknown.” A later implementation should use
`Result<Option<Snapshot>, LlmError>` (or an equivalent explicit resolution
state). Do not map an absent callback, cache miss, or unavailable input to
Native's policy-service-disabled outcome. Those states have different
semantics in the Native source.

## Field-by-field source map

| Snapshot field | Current production source | Coverage and honest status |
| --- | --- | --- |
| `serving_model` | The Agent stream attempt starts with `initial_response_model`, keeps a separate `query_serving_model`, and advances that identity only after an admitted visible fallback. The declined-refusal formatter call receives this current model and the profile. | **Available.** This is the best current input to the source closure; it is the model serving the refused request, not the proposed fallback candidate. Resolve it against the route registry before using provider/model metadata. |
| `model_eligible` | `core::host::model_capabilities` has a baked `refusal_fallback` capability table and `CLAUDE_CODE_MODEL_CAPABILITIES` override parser. | **Partial, not a complete answer.** Native `eYe` also combines current model identity, served catalog/capability observations, Fable/default-model conditions, GrowthBook roster, provider classification, and the current Opus candidate path. The core `has_capability` result and organization `availableModels` policy are not substitutes. `runtime/src/model_resolution.rs` explicitly leaves Native live catalog, Fable entitlement, and broader entitlement facts absent. |
| `display_label` | `llm-runtime::ModelListing` carries configured `display_model`; `ProviderApiAdapter::list_model_listings` projects it to Host listings. `orchestrator::prompt::env_meta::marketing_name_for_model` has a static display-name helper for recognized Claude IDs. | **Partial.** A resolved configured route can identify its profile and request model, then match a full service listing. Static marketing names cover known identities. Native `$o` also uses the baked identity/catalog path and can use a served custom-model name; there is no single production resolver that joins these sources for the route being refused. Do not take the first listing with a matching bare model when profile resolution is ambiguous. |
| `model_family` | Provider model metadata includes `metadata.family`; it flows through `llm-runtime::ModelListing` and Host `ModelListing`. | **Partial.** This is usable when the selected configured profile carries family metadata. The Host baked capability table is not a full Native family catalog, and a missing metadata field does not mean the model has no family. Keep unknown as unresolved. |
| `fable_copy_suppressed` | `core::host::model_capabilities::normalize_model_id` handles several Claude wrappers and `[1m]`; `runtime::model_resolution` captures explicit family-default environment/profile aliases. | **Partial.** The core family/capability and Fable default facts can cover known identities. Native `Dw` additionally consults Native model identity and catalog recognition. The existing `server_fallback_help_url` has a local Fable approximation, but its comment acknowledges that Native's larger alias resolver is not exposed. It is not a complete `Dw` source. |
| `opus_5_5_exception` | Current model string is available; `normalize_model_id` canonicalizes known wrappers and `[1m]`. | **Available from the resolved identity.** Compute the Native explicit `claude-opus-5-5` test from the canonical serving model. Do not infer this from Opus family membership. |
| `help_url` | `orchestrator::query_model::server_fallback_help_url` already maps canonical Opus 5, Fable-looking IDs/default, and a generic URL. | **Partial.** Reuse only after resolving the exact serving identity and Fable default/catalog inputs. Native `b1r` depends on its model identity/catalog rules; the existing helper documents a narrower alias surface. |
| `provider` | Resolved routes carry an explicit open-string `ProviderId`; profiles also carry protocol, auth strategy, credential reference, and endpoint. Desktop/mobile config assembly owns these values. | **Partial.** First-party, Bedrock, Vertex, Foundry, and generic route identities are represented. Native provider classification also has Anthropic AWS, Anthropic Google Cloud, Gateway, and Mantle branches and consults host credential-slot/provider state. The current public `ResolvedRoute` omits auth/protocol/endpoint, and no typed non-secret snapshot exposes all classifier inputs. A base URL, profile label, or auth scheme alone is insufficient evidence for Native `He()`. Do not read or copy credential secrets into the text snapshot. |
| `interactive` | Desktop computes `DesktopSessionComposition::is_interactive_session`; mobile computes `mobile_launch_is_interactive` from `MobileHostEnvironment`. Both pass the value into `ApiService::with_interactive_session`. | **Available at composition.** Capture that exact per-runtime launch value in the closure. A desktop transport/SDK session is non-interactive for these semantics even though it may support interactive permission prompts. Do not use a process-global TTY or permission-support bit. |
| `feedback_eligible` | `core::host::traffic_mode::is_essential_traffic_only` covers the nonessential-traffic gate; raw process environment is available. `telemetry::feature_flags` provides cached GrowthBook flag readers and a generic refresh client. | **Missing as a complete policy result.** No production call site instantiates `FeatureFlagsClient`/`FeatureFlagsFetcher`; its synchronous snapshot begins empty and returns caller defaults. Repo search finds no `allow_product_feedback` consumer or Native-compatible account-policy snapshot for this decision. Native `QA()` checks the raw feedback-disable gates, nonessential-traffic mode, and `allow_product_feedback` policy decision. A generic GrowthBook bool, current command registration, MCP policy, or `interactive` value cannot replace that decision. |

The third (brand) input remains owned by the existing branding/composition
boundary and is intentionally not included in this field audit.

## Existing route and cache seams

The provider-neutral SDK already has the route identity primitives needed for a
safe route join:

- `llm-client::ResolvedRoute` contains `provider_id`, `profile_name`,
  `request_model`, `display_model`, and generic capability support.
- `llm-runtime::ApiService::resolve_media_route(model, profile)` resolves the
  configured route and returns the main `ResolvedRoute` plus any vision
  delegate. `ApiService::model_listings()` returns the configured listing
  snapshot. A route-specific lookup can match both resolved `profile_name` and
  `request_model`, then use that listing's `display_model` and
  `metadata.family`.
- The route profile/catalog has explicit provider ID, protocol, auth strategy,
  credential configuration, model aliases, display name, and provider metadata.
  `ModelMetadata.family` is a provider-published field; it is informational and
  optional, not a routing rule.

`ApiService` currently exposes route resolution and listings but no single
non-secret `RefusalRouteFacts` value combining the selected profile's provider,
protocol/auth classification, endpoint class, route model metadata, and exact
model identity. Adding that value in `llm-runtime` is the smallest route-facing
seam; provider wire/authentication code remains in `llm-client` and must not
move into Orchestrator.

The existing caches are not interchangeable:

- `telemetry::feature_flags` has an empty process snapshot plus a pluggable
  `FeatureFlagsClient` with five-minute background refresh. There is no
  production fetcher construction in Runtime. Its cold default is a caller
  default; it does not report whether Native's account policy service is
  disabled, enabled-but-cold, or has a loaded policy snapshot.
- `llm-runtime::fast_admission::Availability` is an account-scoped org-status
  cache for Fast mode. It is credential-bound and explicitly only answers that
  feature's status query; it has no `allow_product_feedback` decision and must
  not be repurposed.
- The `ModelRuntime` registry and `ApiService.model_listings` are constructed
  from the configured provider catalog. They provide real configured routes,
  not Native's live served model catalog or its separate model-identity/cache
  state.

## Complete implementation path

1. **Route facts in `llm-runtime`.** Add a non-secret method around the existing
   `resolve_media_route` + `model_listings` APIs to return the actual resolved
   profile, provider ID, wire protocol/auth class, display label, model family,
   and exact request/display identity. Do not infer `firstParty` from an URL.
   The provider classifier must be supplied explicit route/credential-slot
   facts from the host's auth/config composition, without exposing token bytes.
2. **Policy facts in the owning Host services.** Add a typed, live source for
   the Native refusal-capability inputs and another for the account
   `allow_product_feedback` effective-policy state. The latter must distinguish
   service-disabled fallback from service-enabled cache miss/deny and from
   “Host has no resolver.” Reuse existing capability-override parsing and
   feature cache where they are semantically exact; add missing served
   capability/catalog or account-policy inputs at their authoritative owner.
   Keep model-allowlist admission separate.
3. **Per-runtime composition in desktop and mobile assembly.** After creating
   `ApiService` and computing the existing launch-interactive bool, attach a
   `RefusalApiTextSnapshotSource` to the adapter. The closure resolves the
   passed serving model/profile against live route facts and current policy
   snapshots, computes the pure Native-specific fields, and returns no
   snapshot if required facts remain unknown. Capture session interaction
   explicitly per runtime; do not read global session flags at refusal time.
4. **Adapter and consumer stay thin.** `ProviderApiAdapter` should forward the
   resolved source result to both main and subagent APIs; Agent runner remains
   the caller for its declined refusal row. The sibling `query_model.rs`
   server-fallback banner is a separate projection and currently passes
   `feedback_available: false`; share a future authoritative policy resolver
   only after that resolver exists, not the refusal row's constructed snapshot.

This path keeps all provider request/auth work in `llm-client`, route identity
and provider-neutral selection in `llm-runtime`, and Native eligibility/account
policy composition in Host/runtime services. Until the required facts arrive,
the existing unattached-source `None` is the honest state; it is not a Native
disabled-service result.
