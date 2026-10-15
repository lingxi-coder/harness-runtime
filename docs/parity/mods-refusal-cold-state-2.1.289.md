# Claude Code 2.1.289 refusal inputs in cold state

This audit separates Native cold defaults from missing live data and from a
Host resolver that has not been connected. It uses exact source windows from
the pinned 2.1.289 binary and an executable controlled-source probe:

- [Probe](../../scripts/tests/mods_refusal_cold_state_289_probe.mjs)
- [Receipt with all 42 source ranges and executed outputs](mods-refusal-cold-state-2.1.289.json)

Run:

```sh
node scripts/tests/mods_refusal_cold_state_289_probe.mjs /tmp/harness-claude-code-2.1.289-darwin-arm64
```

The binary SHA-256 is
03d66745e3bb69ec727d66023696f3820bc0a00a8a5ba725eb6706d0c67cbe69.
The probe executes Native source with fixed host, credential, catalog, feature,
and policy-cache inputs. It does not launch Claude Code or access an account,
network, user configuration, or provider. GrowthBook's feature-schema parser,
credential reads, the policy disk-cache reader, and the model-string Host
object are controlled seams; the receipt states their exact values.

## Model capability and catalog defaults

Native's IL() state starts with servedCapabilityLookup, featureGateLookup, and
modelKnowledge unset. The model module registers the served capability callback
during module initialization. The per-host catalog initializer starts with
servedCatalogActive and servedCatalogDeactivatedRows unset. With no active or
deactivated rows, the registered callback returns undefined; fy then falls
through to static Hsr metadata. This is a miss, not a deny. An explicit
matching CLAUDE_CODE_MODEL_CAPABILITIES false is checked first and still denies.

The executable case starts before registration, checks that the resolver is
absent, evaluates Native's registration statement, and then observes a
registered callback returning undefined against the cold catalog. It also
observes the static Sonnet 5.5 capability, family, and display name, verifies
that the cold refusal policy accepts that baked capability, and verifies an
explicit environment deny rejects it. The Mythos case is rejected with an
empty roster.

The model fallback candidate is also not simply an environment constant.
Native's model-string getter reads the Host modelStringsCache; when it is null,
jS() initializes the provider model table through ro(He()) and then applies
modelOverrides. With the controlled first-party route and absent overrides,
the baked opus5 alias resolves to claude-opus-5, which becomes the KI
candidate. This is the Native cold path for those explicit inputs, not a
universal current candidate: a populated model-string cache, provider-specific
route, or user override changes the result.

The embedded Hsr catalog is available independently of the served catalog.
Thus a cold built-in model can have a static label, family, and capability.
Unknown custom model labels and runtime capabilities still need the served
catalog/identity sources. ANTHROPIC_DEFAULT_FABLE_MODEL is a separate
environment equality check; the baked Fable alias does not populate that
environment field.

## GrowthBook roster

Native Gr() asks for k("tengu_loggia_roster", []). The GrowthBook client's
exact getFeatureValueWithSource method returns the supplied fallback:

- source disabled when the feature service is disabled and its disk cache is
  not readable;
- source fallback when enabled but neither remote values nor disk data has the
  key;
- source disk when the service is disabled but Native's disk-read gate is
  enabled and a cached key is present.

So [] is the actual result for a cold lookup with no available value, but does
not establish that the roster is permanently empty or unavailable. A later
refresh or a readable persisted feature may change lQn. The controlled probe
executes these three Native feature-lookup branches and runs exact Gr/lQn for
the cold [] value; only the Zod array schema implementation is stubbed for
already typed array inputs.

## Feedback policy and service state

Native policy state p4o begins with sessionCache = null,
lastFetchOutcome = null, failOpenVerdict = false, hintedDeniedKeys = [],
and hintedTaints = []. dS() returns null both when the policy service is
unavailable and when it is eligible but no session or disk snapshot is loaded.
Those states are distinguishable through C_(), not through dS() alone.

C_() means bK() === undefined. The pinned ter/C_/bK source accepts the
first-party route only, rejects a custom endpoint, then accepts an API key or
OAuth with an access token, the imported inference scope, and an enterprise or
team subscription. It rejects missing auth, missing scope, and prosumer OAuth.
The probe supplies a sentinel for the imported scope constant; it does not
assert the literal scope name. He() classifies provider from Native
credential-slot state and provider environment flags; it does not branch on
API-key versus OAuth auth type.

With the cold p4o state and no local hints:

| Native state | dS() | sn("allow_product_feedback") / QA() |
| --- | --- | --- |
| Policy service eligible, but no session/disk snapshot | null | false |
| Policy service unavailable, no snapshot or local hints | null | true |

The first row follows allow_product_feedback metadata
serverPopulated: true and onCacheMiss: "deny". The second row is Native's
disabled-service fallback; it must not be generalized to an unattached Host
resolver. In both cases the service-status input is actual Native policy
eligibility, not whether a Host callback exists.

Nre() checks raw truthiness of DISABLE_FEEDBACK_COMMAND, then
DISABLE_BUG_COMMAND, then Native nonessential-traffic mode before it calls
dp("allow_product_feedback", ...). Therefore strings "false" and "0" disable
the command when passed through unchanged.

## What the current Harness can claim

The current Rust snapshot boundary keeps missing composition distinct from a
resolved false: [ProviderApiAdapter::new](../../crates/orchestrator/src/provider_adapter.rs)
initializes its optional RefusalApiTextSnapshotSource to None, and
resolved_refusal_api_text_snapshot returns None when no source is attached. A
source error is also propagated. The [ConversationApi default](../../crates/orchestrator/src/conversation.rs)
likewise returns Ok(None). A source search found no production call to
with_refusal_api_text_snapshot_source; its uses are the adapter tests. The
refusal formatter is invoked from the [Agent runner](../../crates/agent/src/runner.rs),
which records the absence instead of fabricating a Native refusal row.

The separate ordinary server-fallback notice in
[query_model.rs](../../crates/orchestrator/src/query_model.rs) currently passes
feedback_available=false because its facts source is not connected. That is an
explicit Host placeholder for that notice, not evidence that Native QA() is
false and not a refusal-text snapshot.

This means a Host with no snapshot source is in a third state: Native facts
were not resolved by this Host path. It is neither Native's C_() == false
state nor an enabled Native service with a cold policy cache. The current
RefusalApiTextSnapshot fields model_eligible and feedback_eligible are
booleans, so a future builder should not manufacture a snapshot from unknown
facts and encode unknown as false or true.

Facts the Host may use only after resolving the corresponding source:

- The current serving model and launch interaction mode come from the active
  query/launch route.
- Built-in family, label, and baked capabilities can use the Native-parity
  static catalog after resolving canonical model identity.
- Provider kind must come from the actual provider/credential route. A profile
  name or endpoint URL alone does not prove Native He() identity.
- eYe still needs the actual capability environment, served capability and
  catalog state, model identity/settings, GrowthBook roster state, provider,
  and current Opus candidate inputs.
- QA still needs the raw disable/nonessential-traffic inputs plus the
  effective allow_product_feedback policy state. An unattached Host resolver
  must remain absent until those facts are acquired.
- Brand strings and dynamic custom-model label/help data need their actual
  sources; the cold static catalog does not define them.

These results do not authorize substituting organization availableModels,
the generic model picker list, provider URL heuristics, GrowthBook's empty
fallback for an unqueried live roster, or the Host's missing callback for
Native's service-disabled policy result.

## Key source pins

The full receipt lists all 42 verified windows. Core cold-state ranges:

| Source | Byte range | SHA-256 |
| --- | --- | --- |
| Static Hsr catalog | [180307020, 180324545) | f87f1b4aef70a13ec78e3a9b6f38bd1db7c67ef7ed2b3ec5c21e037816771b1b |
| Initial IL resolver state | [180324545, 180324763) | 4109ccec64d48bef1a02be7cf78b2311bf0eae008f1436561a0a1bafee81806d |
| Per-host served catalog initial state AA/Hr | [181696189, 181697613) | cf260d4711c9341dcc7ff6a48521bb21445ef4cccad93b64dee0e7028088bece |
| GrowthBook feature lookup | [181846341, 181847084) | 106782695c7016daf255548f5737e6472e2bb5bb1b860e58d9eae8b253220113 |
| GrowthBook lazy client setup | [182188260, 182188813) | 1aae4728aa575c9f6400393b8f393b0ce37fde222381e02a761e755ee77f7bd9 |
| Native policy eligibility ter/C_/bK | [183094674, 183095246) | a8d1c9b012dfa732e5ddb810cd6839e26b54f5184089a0953c25950f5927ae56 |
| Initial policy session p4o | [183088968, 183092134) | 8e48921f6ac419646f6c20e9baad6a5c7fa4b5c08e493df4266447e8b0d2e590 |
| dS() policy snapshot lookup | [183115886, 183116056) | c95de8b8d86e7b3e67af30e461be4938b2cc8b9ed26216f45d5e2407ae7d90c4 |
| allow_product_feedback metadata | [183104513, 183110955) | d63179cd87ac8debe86eb11f1d2072dc577a4ce2723db985c628a596eb91132c |
| Effective policy sn/dp | [183109600, 183114613) | c56fedb4ac86ae2e1ac159a456be6156dcd571f09814ac9aede87761e9b34947 |
| Nre/QA feedback command gates | [186171443, 186171956) | 5f5a000f728ef8cea9c617397a1334ea1729022b0e90aaf5d21bfbb91974f872 |
