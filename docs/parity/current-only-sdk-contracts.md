# Current-only SDK contracts

The latest-Claude-Code alignment work does not support older Harness APIs or
private persisted layouts. These changes intentionally require current callers
to implement the complete capability instead of retaining a forwarding shim.
LingXi branding, multiple LLM providers, Fusion, and Create Local App retain
their authorized behavior.

## Model execution

Anthropic wire policy uses the current `AnthropicRequestKind::{Main, SideQuery}`.
The kind is host execution context and never model input or persisted data;
dispatch category does not select the body policy. `AnthropicRequestPolicy::apply`
returns `Result<(), LlmError>` and mutates the body, headers, exact UTF-16 string overrides and URL together, before
sealing/signing. SDK callers pass all four buffers; no old three-buffer overload remains. The former public standalone
`merge_extra` entry point is private, with no forwarding overload. Main explicit
config fields win over automatic fields; side extra config replaces the whole
automatic config. The host chooses flags/routes; the SDK owns wire merging.

`sanitize_extra_body` removes native AFK beta tokens and the top-level `tk`
member of JSON metadata identity strings before automatic feature decisions.
Each physical preparation owns the sanitized snapshot. Invalid JavaScript
string coercion is a typed request error and cannot reach transport. The host
strips a single leading BOM while parsing the environment object. Ordinary
experimental-beta disabling still permits extra body; HIPAA context admission
remains outstanding.

`message_header_parameters` selects the Messages/Foundry SDK parameter path.
It extracts `betas`, `user_profile_id` and `workspace_id` into headers and
removes their body fields and exact-string sidecars. The host skips automatic
beta-header assembly when explicit extra `betas` survives sanitation, including
null. Null retains authenticator defaults; a non-null beta overrides them.
Bedrock/Vertex retain their provider-specific body policy. There is no old
infallible apply overload.

The SDK now orders the computed main/side base fields by native `pd`/`En`
templates before extra-body collision handling. Main computed speed/thread/
diagnostics and the final stream spread keep native tail and collision positions.
Existing fields are ordered; missing caller fields are not invented. Complete
caller defaults and context admission remain separate work.

Messages/Foundry parameter policy also applies `beta=true` to the selected URL.
Both streaming draft paths synchronize the final URL before signing, alongside
the existing non-stream seal path. Other transports retain their provider URL.


`normalize_message_parameters(body, headers, exact_string_overrides, url, mode)`
is the SDK-owned current Messages SDK parameter conversion. CountTokens appends
`token-counting-2024-11-01` after iterable betas and sets `beta=true` on the counting
URL before signing. A string beta argument spreads by Unicode code point; a
noniterable value is a typed request error. Generation-to-count adaptation still
removes generation-only typed fields. This does not claim native CLI counting,
request-options override precedence or cloud counter acceptance.

`AnthropicEffortPolicy { supported, value }` now carries the selected native
YMe input rather than inferring it from a codec-computed field. Main explicit
config wins over the selected value after unsupported effort deletion; side
extra config is spread after automatic resolution. Numeric/boolean/object
resolved values produce no effort field, while undefined preserves the native
supported main beta. Replaced/deleted effort discards its previous exact-string
sidecar and retains unrelated format strings.

Core's pure `host::effort` implements current iF/$9/jb/T precedence and cap
normalization. Trusted `ExecutionContext::effort_state` carries turn, hook,
carried, managed/flag/override/catalog defaults and applicable caps; it is never
model input or persisted authority. The native service selects resolution before
SDK validation, removes its typed effort from a clone, and pins the selected
value to one physical preparation. Authentication cannot change that value;
a fresh preparation sees a changed environment. The authorized brand uses
`LINGXI_EFFORT_LEVEL`, with current native auto/unset/med/parseInt semantics;
there is no old environment alias. Main resolves its high default, side queries
without hook effort do not inherit it, and Opus 5 disabled-thinking side effort
is held at high. Raw ModelRuntime callers retain provider-neutral SDK controls
unless the host explicitly selects native service resolution.

`EffectiveSettings::effort_layers` retains admitted sources before merging.
`ExecutionContext::effort_settings` is an explicit optional snapshot: `Some([])`
clears settings caps and never authorizes ambient file discovery. Native K(e)
uses canonical identity equality and the minimum across these sources; per-model
`max` replaces and exempts that source's global cap. The service's host-owned
settings callback is sampled once before each physical preparation. Desktop
ordinary/headless and mobile composition bind their explicit configured paths
and allowed sources; the SDK continues to own all model communication.

Explicit Foundry deployment facts survive the host SDK projection. Effort
support/caps use the underlying model identity; the wire deployment is retained.

The current mutable effort contract is `SessionEffort::{Inherit, Default, Level}`.
`OrchestratorApiClient::set_effort` requires that enum; the Option setter is gone.
An absent launch value inherits, an explicit automatic choice selects Default,
and a level retains its scalar value. Native `Q/ee/kd` maintains per-model defaults
and present undefined suppression, with canonical-entry preference and native
integer-key enumeration. Effective policy defaults merge within one slot; K still
consumes each original tier. ModelSettings uses insertion order and prototype
filtering before typed entry parsing.

`ApiService::with_effort_settings_source` takes a boot snapshot for inherited
defaults and samples current caps before physical preparations. Session state is
pinned alongside those inputs before credential callbacks. Default skips the boot
table, side queries without explicit hook effort do not inherit it, and missing
resume effort selects Inherit. Host composition supplies first-start options from
an admitted home; empty injected defaults do not authorize ambient discovery.
The generic structured reasoning loader has no native root effortLevel fallback;
current native root consumption is handled by the table itself.

The source-pinned K fixture supplies canonical identity state; it is not full
Rt/Be/catalog identity acceptance. Served
catalog acquisition/capability overrides, carried-effort lifetime and command
display/persistence consumers remain part of the full alignment goal.

`ReasoningTarget` now requires selected `features` and `inference` facts alongside
route identity. Directory observations and connection restrictions determine
controls; unknown values retain the provider preset. Explicit empty effort lists
and negative per-level facts remove controls. The historical Opus 4.5 host-only
exception is removed. No previous target initializer or forwarding overload is
retained. `RoutingCatalog::profiles()` exposes the immutable selected snapshot
for host listing consumers.

Native physical preparation uses the selected SDK model's effort support,
max/xhigh observations and runtime level list in current jw/J/pQ/xXe order.
Configured/catalog defaults enter the catalog slot only when no explicit host
catalog default exists and the level belongs to the native five-level vocabulary.
Earlier default sources and explicit session selections keep their priority.
The current native baked defaults remain available when the SDK has no model
default. The 21-row native inventory also locks SDK Opus 4.7/5.5 launch effort
facts. A host display-label change preserves unique wire-model facts without
guessing among duplicate rows. This does not invent Models API default/ceiling
fields or claim native account-cache/bootstrap acquisition.

Host model admission uses the 2.1.287 chronological `av/lr` comparison and built-in
wire-ID canonicalization. Foundry is a distinct beta provider. Main automatic
effort includes the supported default case; explicit main effort suppresses its
insertion. Dynamic served model metadata and HIPAA taint remain separate gates.

Metadata identity cleanup uses an SDK-owned arena parser and explicit stacks.
Unpaired UTF-16 keys/leaves, duplicate-key last writes, numeric overflow and deep
arrays retain native JSON semantics while deleting only the root `tk` property.
Invalid/nonobject/no-tk identity strings keep their original bytes. This closes
identity-string parsing, not the outer arbitrary extra-body parser.

`exact_json::serialize` requires an explicit `JsonEncoding::{Serde, JavaScript}`.
The old two-argument signature is gone. JavaScript mode uses `ryu-js` 1.0.3,
IEEE-754 number semantics and canonical array-index object enumeration; Serde
mode retains the other providers' numeric types and formatting. Exact strings
require canonical pointers, including canonical array indexes and escaped keys.
The SDK's borrowed wire encoder and body-length counter use the same mode
without materializing text/schema/base64 payloads. Each final draft serialization
selects the effective protocol's mode before signing.

`ProviderRequest` now requires `json_encoding` in its current JSON envelope.
A missing field is rejected; there is no legacy deserialization fallback.
Preparation and codec projection set JavaScript for the four Claude protocols;
OpenAI/Gemini retain Serde. Raw authenticated bytes still take precedence.

The current native SDK `px` normalizer still consumes truthy `output_format`,
retains other output-config fields and rejects an existing truthy format with
the native error message. It preserves the config's body position and remaps
exact strings to the final format path. This is current native behavior, not a
forwarding Harness API for old versions.

`SubagentApiClient` has one required method:

```rust
async fn stream(&self, request: SubagentApiRequest)
    -> Result<BoxStream<'static, Result<HistoryEvent, LlmError>>, LlmError>;
```

The owned request includes model, profile, system, messages, tools, forced tool,
effort, and `SubagentApiCallOpts`. Registered model attempts, output ceilings,
and query-source accounting remain explicit. The former eight-method default
chain, which could discard routing or execution options, has been removed.
Test clients explicitly generate event streams from their scripted responses.

`OrchestratorApiClient` likewise has one required current method:

```rust
async fn messages_create(&self, request: OrchestratorApiRequest)
    -> Result<HistoryResponse, LlmError>;
```

`OrchestratorApiRequest::Main` owns a `MessagesCreateRequest`; `HookPrompt`
owns the isolated evaluator inputs. The five default message forwarding
methods are gone. Main options carry the output ceiling, context-hint offer
and independent beta activation, explicit overload seed, ordered fallback
policy, registered attempt, and query-source accounting. `Some(0)` retains
an explicit streaming-fallback identity; ordinary requests have no seed.
`ApiService::messages_create` consumes the same owned main request. It no
longer exposes the four option-specific message methods or accepts ignored
subscription flags; retry gates read the live subscription snapshot.

Main assembly retains the session's structured-output and thinking policy.
The producer accumulates output ceilings, active context-hint offers and
configured fallback independently, including below-floor offers with no body.
Hint rejection, context collapse and reactive-compaction retries preserve the
ceiling and fallback while rebuilding hint parameters for their new history.
Scheduled assembly retains its captured route, thinking, effort and auxiliary
header class while honoring the request's output ceiling, hint, seed and
fallback. Hook evaluation always requests the evaluator JSON schema with
thinking disabled, including when a schedule is active. Every physical call
still passes through preparation, authentication/sealing, registered-attempt
admission, the original watchdog, and shared retry accounting.

Dispatch headers classify an explicit query source with the native current
categories: `repl_main_thread*` and `sdk` are main, `agent:*` and `hook_agent`
are subagent, and other named sources are auxiliary. Hook and scheduled
streams therefore omit v2s when v2d is off; they do not inherit a main header
merely by streaming. With an absent query source, a canonical non-streaming
call retains the caller's explicit Main/Auxiliary scope. That extension does
not invent a source label or change model-input policy.

Context-hint parameters are unwrapped once at the conversation boundary.
The model body receives the hint value, without a nested `context_hint`
envelope. A below-floor offer activates the native beta without adding a body
field; activation is scoped to that logical request. Token-count, catalog,
media, diagnostic, mutable session policy and WebSocket lifecycle hooks keep
their current optional capabilities. Their absence does not forward a model
request while discarding its execution controls.

Model/count authentication captures credential material once per current SDK
draft, including public preparation, prewarm and HTTP fallback. The preview
`prepare_at` timestamp remains explicit; final body/header images are re-signed
at dispatch time. New drafts refresh credentials, and long-lived file services
remain live. No signed request image or credential material crosses drafts.

Tool choice is local to that request. A child which does not request a forced
tool clears the main session's StructuredOutput choice; an explicitly forced
child uses its own tool. Main schema execution retains its configured choice.
Default Anthropic `auto` is omitted from the mutable prepared body before
authentication and sealing, preserving the native bytes and the final signed
body. Other protocol families and explicit choices retain their own semantics.

An unconfigured subagent API produces a startup failure before discovery,
hooks, or transcript writes. It releases the existing restoration and pool
cleanup paths. The reducer stub no longer produces synthetic completion.
Similarly, an unconfigured `ForkedAgentRunner::run` returns
`ForkError::MissingBackend`; it cannot supply a fake assistant answer or zero
usage success to compaction or memory extraction.

## Instruction and identity ownership

`RootInstructionContextProvider` must bind to the exact owning root
orchestrator. The raw `instruction_context_provider(memory)` factory is gone.
Main and ordinary child reads use the same current root Gv; ManagedOnly changes
only `instructions`, retaining the chosen Full context's other fields and order.

`MemoryHierarchyProvider::load_conditional_rules(cwd, trigger, mode)` is a
required current capability. Implementations explicitly provide fresh
conditional rules or their actual injected data. No default bridge reloads an
unconditional eager file snapshot. Sent-rule deduplication remains separate
from fresh rule acquisition.

`AuthHandle::register_account_change_observer` is required. Observers are weak,
and successful first-party credential persistence synchronously invalidates
the owning context through pT. Failed persistence does not emit the event, and
account changes retain the eager file cache.

Reporting admission belongs to the root, which already owns task notifications.
`TaskRegistry::bind_reporting_admission(Weak<dyn ReportingAdmission>)` is the
required current API; the former strong Arc signature has no compatibility
wrapper. Desktop, mobile, and test callers retain the independent root owner
and explicitly downgrade it when binding. The registry no longer closes the
root/task-notifications/registry ownership cycle or retains its durable writer
after shutdown. Scope lookup and admission temporarily upgrade the owner for
their awaited transaction, including receipt commitment. When the root cannot
be upgraded, scope is absent and delivery is rejected without a receipt.

The v5 full validation passed Runtime 1,386 tests (1 ignored), LLM Runtime
1,030, Orchestrator 1,277, and Tasks 534. It includes both root lifetime
regressions and the real desktop drain plus three resume regressions, with the
existing immediate release assertions intact. Evidence:
[`harness-286-current-weak-admission-runtime-full-v5.log`](/tmp/harness-286-current-weak-admission-runtime-full-v5.log).

The active sibling app still consumes its existing canonical Git revision.
These SDK edits do not silently change that pin. Its four custom AuthHandle
test implementations must add the required observer registration when that
revision is intentionally advanced; they were not patched against an older
pinned trait.

## Current native formats

Compact boundaries require `system` with subtype `compact_boundary` in both
history selection and API normalization. A plain system message whose body is
`Conversation compacted` does not truncate earlier messages. No old-layout
migration is provided. File, compact-reference and nested-memory attachments
persist their current raw payload with one UUID; current cold/hot replay shares
the same projection instead of parsing old rendered prose. The obsolete
post-compact UTF-8/character-clamp reader and its exported clamps are removed.
Current tool-reference validation uses native 2.1.287's eight aliases. The four
obsolete Output-to-TaskOutput mappings are gone; explicitly available current
tool names still validate by their own name. Recovery source classification
accepts `agent:*` and `hook_agent`, without the old bare `subagent` alias.

The latest native MCP `initialize` fallback, ignored `mode`/`team_name` wire
fields, and native transcript representations remain current reference
behavior. Their presence does not authorize an older Harness compatibility
adapter. Byte/state acceptance and unimplemented native capabilities are
tracked separately in the version-specific parity documents.

Dispatch recovery in 2.1.287 is per query and uses `v2p` on the recovery
attempt. The host owns route/feature/retry decisions; `llm-client` owns all
model communication, request sealing/signing and network transport. Current
multi-provider support is required. The user authorizes modifying the SDK
when required; obsolete APIs and forwarding compatibility remain excluded.
