# Claude Code 2.1.287 byte-level alignment

The official latest advanced to **2.1.288** at the final recheck of this wave.
This document retains validated **2.1.287** evidence; the current target audit
continues on 2.1.288.

The 2.1.287 target was refreshed on 2026-10-01 after the official latest release
advanced during validation. Current APIs only: no older Harness forwarding
chains, migration aliases or silently dropped request options. The authorized
brand, multi-LLM provider, Fusion and Create Local App differences remain.

Wave sections retain their original validation scope and open items; the newest
wave appears first and records which earlier gaps it closes.

The current OAuth-source wave implements native `rk/xk/vK` selection in
`llm-client`: parsed environmental tokens, actual environment scope splitting,
and supplied descriptor/store snapshots. The [source oracle](../../scripts/tests/oauth_source_287_oracle.mjs)
executes **1,744** actual source-pinned cases. The environmental default is only
`user:inference`; descriptor defaults additionally include CCR inference and file
upload. A descriptor's explicit empty scope list remains empty. Background
snapshots yield to saved OAuth unless the host-managed predicate applies. BOM is
JavaScript whitespace; NEL is not.

The desktop credential factory now boot-captures environmental OAuth token and
scopes together and skips stored OAuth acquisition while an environmental token
is present. API-key source markers remain attached when a key wins, so error copy
can name the actual credential setting. The selected credential reaches
the normal composite and Copilot wrapper, then the SDK authenticator and both physical model drivers. Environmental
credentials carry no invented expiry/refresh token and have no renewal capability.
Subscription/rate-limit metadata use branded environment names from `branding`.
The current model-source resolver is retained; its complete latest producer
precedence is still an open audit.

An isolated child owns environment mutation and compares all **1,744** rows against
the environmental credential provider. Twelve real desktop-factory/SDK scenarios
cover default versus explicit profile grants, API-key precedence and both model
drivers. The store fails immediately if an OAuth read occurs; writes are counted.
Fast status requires the actual profile grant, and OAuth/header/speed/beta bytes
are checked at the captured SDK transport. Other providers retain their routing.
Mobile keeps its host-configured credential path; no new ambient OAuth read is
introduced there.

Current validation passed **5,076** tests and **40** checks, with **1** existing manual runtime test ignored. Evidence and **12** unchanged source hashes are recorded in `oauth_source_current_validation`. Actual descriptor/socket/
background-slot acquisition, a status OAuth snapshot independent of the selected
model credential, source changes and account lifecycle, startup/persistence and
atomic dispatch admission remain open. The native helper oracle is a source VM
comparison, not embedded Bun/full-CLI acceptance. Mods remain deferred to the other
branch.

The current session-identity wave acquires Fast session-token, no-user and
agent-owned-remote facts from the actual process environment. These are distinct
from the model OAuth credential and granted scopes. The SDK owns native
`lc/Zvn`, `wU/Io/Fyt/VJn/e_` and `ma/Pz/Ln` protocol decisions, with a borrowed
cached-token slot available to hosts. Runtime desktop/mobile policy factories
consume the environment path; FD/socket/background-slot acquisition remains open.

The [session oracle](../../scripts/tests/session_identity_287_oracle.mjs) executes
**1,478** actual source-pinned cases. It includes parsed environment bool/string
semantics, raw session-token truthiness, prefixed/permissive base64url payloads,
service and BYOC worker identities, presence of null account keys, and agent-owned
remote entrypoints. JSON classification preserves large numbers, lone UTF-16
surrogates, deeply nested unknown fields and duplicate-key outcomes; malformed
grammar still fails. Only string/presence facts are retained, with no signature
verification or raw token/claim Debug surface. This matches native classification,
not a new credential verification protocol.

An isolated runtime test process owns all environment mutation. It compares
**1,463** actual environment inputs against the SDK/native matrix and runs **276**
real SDK status-prefetch scenarios from the production policy factory. No-user
identities skip before local credentials and status GETs. Both physical model
drivers retain an earlier server enable for an owned remote session. A stale
local persisted-enabled guess plus flag opt-in cannot manufacture that server
permission. An OAuth token alone is a negative control for session identity.
The status and model transport remain in `llm-client`, and other providers keep
their existing credential/runtime paths.

Current validation passed **5,073** tests and **37** checks, with **1** existing manual runtime test ignored. Evidence and **8** unchanged source hashes are recorded in `session_identity_current_validation`. Full model OAuth
environment/FD scope acquisition, cached session-token producers and lifecycle,
other remote/session facts, startup/persistence and atomic dispatch admission
remain open. Embedded Bun/CLI, live-provider and device acceptance are not
established. Mods remain deferred to the other branch.

The current Fast auth-source wave implements the native `Rbe` status-query
selection: an admitted OAuth token takes priority, otherwise an alternate API key
is used. The [auth-source oracle](../../scripts/tests/fast_auth_sources_287_oracle.mjs)
executes **576** actual `g` selections, **54** `mb` key-source cases, **6** `NQn`
cache reads and **6** `Xb` safe lookups. Trust/environment/managed/store acquisition
is supplied to those helpers; complete native acquisition remains open.

The production composite exposes an API-key snapshot through the selected
first-party credential scope, including the desktop Copilot wrapper. A configured
helper shadows stored keys even when its cache is cold. The snapshot reads existing
helper data without executing or checking its refresh TTL. Already resolved host
keys and the current canonical secure-store key remain available. Model credentials
continue through their own request-owned SDK snapshot. Compatible and other-provider
routes do not consult this alternate first-party source.

After an admitted refresh attempt completes, the bounded second query reselects
from the current OAuth grant and the previously captured key. Losing profile scope
can therefore select that key; a completed attempt without a new token still runs
the second query. Permission-error classification consumes the new eligibility.
The account remains OAuth-owned for cache binding and native refusal text even
when the status endpoint uses the alternate key.

Seven transport scenarios run through both physical drivers, including narrow or
empty grants, downgraded scopes, server refusal, unchanged-token renewal, missing
OAuth and an unavailable alternate source. They verify GET/exchange/GET ordering,
bounded refresh, status headers, original/renewed model authorization, refusal copy
and final speed/beta. A missing model credential cannot borrow the status key.
The obsolete `provider-key-anthropic` storage-recovery path is removed: lookup,
existence checks and deletion use the current canonical Anthropic storage account.
The current logical provider IDs and multi-provider routing remain supported.

Current validation passed **5,070** tests and **36** checks, with **1** existing manual runtime test ignored. Evidence and **9** unchanged source hashes are recorded in `fast_auth_sources_current_validation`. Full trust/source/remote identity
acquisition, login source changes, startup/persistence and atomic admission through
physical dispatch remain open; embedded Bun/CLI and live-provider acceptance are
not established. Mods remain deferred to the other branch.

The current Fast OAuth wave carries the actual access token and granted scopes
in `Credential::AnthropicOAuth`, captured under one token-state lock. Generic
bearer credentials remain provider-neutral. Fast status admission no longer
uses subscription tier as a substitute for `user:profile`. The source-pinned
[OAuth oracle](../../scripts/tests/fast_oauth_287_oracle.mjs) executes **64** actual
`zp/qu/YBe` scope cases and **30** actual `Rbe` refresh-trigger cases. Session-token
and no-user predicates are supplied facts in those helper cases.

Desktop/mobile restore pass the saved grant scopes into the current required
`AuthState`/driver initialization contract. Granted scopes remain distinct from
the canonical requested refresh scope set: restoring an inference-only grant
must not manufacture `user:profile`, and a later refresh captures its real new
grant. The current constructors have no previous-argument forwarding overload.
Credential/driver regressions exercise this distinction and canonical refresh
request behavior.

The SDK owns status GETs and exact 401/string-revocation-403 trigger classification.
An eligible profile-scope token permits one explicit single-flight renewal through
the selected credential provider, followed by one status query with the renewed
token. Both composite OAuth dispatch and the Copilot wrapper preserve this
capability for the selected Anthropic source. Other sources report unsupported
renewal without implicit model replay. Endpoint construction, token exchange and
model transport remain in `llm-client`.

Real SDK/credential-driver transport cases cover 401, the specific revoked-token
403, a second refusal after renewal, and an inference-only token. They verify
GET/refresh-POST/GET ordering, the one-renewal bound, no status request for an
unadmitted local scope, actual new model authorization and speed/beta policy.
The subscription seed is false in these cases, so a plan-tier shortcut cannot
make them pass. Ordinary same-account renewal and account-generation invalidation
remain separate behaviors from the previous wave.

The alternate API-key source fallback when OAuth is ineligible, complete
session-token/JWT/no-user producers, full login/account source switching,
startup/persistence, refresh scope changes that require those alternate sources,
and atomic admission through physical dispatch remain open. This is not full
`Rbe`, embedded Bun/CLI or live-provider acceptance. Current validation passed **5,024** tests and **35** checks,
with **1** existing manual runtime test ignored. Evidence and
**22** unchanged source hashes are recorded in `fast_oauth_current_validation`. Mods stay deferred.

The current Fast credential-binding wave reuses the SDK draft's existing
request-owned authentication snapshot before the service's final body/header
policy. The status-cache observation is selected by profile, API-key
fingerprint and account generation. An API-key change without an explicit
toggle cannot silently borrow the prior credential's server permission. Ordinary
OAuth renewal preserves the same account permission and prefetch window; login
and logout advance the account generation. An
account event while the credential lookup is pending also excludes that old
observation. Signing consumes the same draft snapshot instead of loading a
second credential. Standard requests and compatible/cloud/non-Claude routes do
not perform this Fast capture. No new status GET is issued by message preparation.

Six real SDK transport scenarios exercise both physical drivers: changed
credentials, a credential store that would rotate if signing performed a second
load, and an account event during the draft lookup. They compare the signed
API-key header, speed, Fast beta, exact lookup count and GET/POST count. Existing
cache tests also check the bound path preserves a server refusal and the native
agent-owned remote answer when local user credentials are absent. Native
`SF` transient-unknown/explicit-opt-in behavior remains unchanged; this binding
does not claim all unknown states are refusals. Cached startup metadata without
a credential identity remains native guessed input, not new account acquisition.

This closes the supplied request-credential check through the final policy and
SDK signing boundary. Account changes after that synchronous policy boundary,
full atomic admission through physical dispatch, OAuth scope/renewal and startup
prefetch/persistence remain open. This is also not live-provider or embedded
Bun/CLI acceptance. Current validation passed **3,789** affected-component tests and
**34** checks, including **1,391** runtime tests and **5** production prompt byte
locks. The runtime's **1** manual catalog test remains ignored. Evidence and
**11** unchanged source hashes are recorded in `fast_binding_current_validation`. Mods remain deferred.

The current Fast-admission wave executes **1,368** actual source-pinned
`SF/a8/ZO/qh` cases, covering policy precedence, SDK opt-in, transient guesses,
authoritative server refusals and agent-owned remote decisions. The separate
[status oracle](../../scripts/tests/fast_status_287_oracle.mjs) executes the actual
`ox` GET options and `nx` normalization (**10** request cases and **11** reasons).
Dynamic kill-switch text, model allowlist facts, SDK session kind and credits
copy are supplied to these helper oracles; their complete acquisition is open.

The SDK owns the status endpoint, API-key/OAuth headers, ten-second deadline and
response normalization. The host owns a selected-profile cache with a
thirty-second prefetch window and credential fingerprint invalidation. Explicit
enable awaits its observation; main adapter admission and final message
body/header enforcement consume it. A compatible multi-provider route does not
query the first-party endpoint. Managed `fastMode:false` remains distinct from
flag opt-in, and a server preference refusal cannot be bypassed by that flag or
the branded skip-organization setting. Detailed refusal text reaches the public
orchestrator toggle. Off remains available when enable is refused.

Desktop and mobile register account observers. Account changes discard cached
admission and reject late in-flight results, including stale persisted-enabled
fallbacks. Cache regressions preserve authoritative refusals across fetch errors
and credential loss, and preserve an agent-owned remote session's earlier server
enable when local user credentials disappear. The production roots currently
supply local-session facts; full remote ownership acquisition remains open.
Transport regressions verify credential rotation, throttling, observer invalidation,
managed policy precedence and removal of both speed and Fast beta. Model/environment
fixtures from the previous wave supply organization permission explicitly and
remain evidence for that narrower gate.
Mobile persistence regressions likewise supply cached organization permission;
a real submission regression verifies that refused enable saves no enabled
preference while Off remains available. Device preference restart/headless/rollback
coverage does not establish native startup organization prefetch or persistence.

Full startup prefetch/persistence, forced OAuth refresh on 401/revocation, OAuth
scope/SDK-session acquisition, current catalog-backed allowlist/default-model
resolution, command model promotion/hooks, dynamic credit copy, cooldown/server
rejection learning and atomic credential-to-dispatch admission remain open.
This is partial account lifecycle acceptance; actual embedded Bun/CLI and live
provider acceptance are excluded. Mods remain deferred to the other branch and
the full alignment goal stays active. Current validation passed **4,946** tests and **34** checks,
including **401** SDK regressions, **1,391** runtime tests and **5** production
prompt byte locks; the runtime's **1** manual catalog test remains ignored.
Evidence and **28** unchanged source hashes are recorded in
`fast_admission_current_validation`.

The current Fast-environment wave connects the branded
`LINGXI_MODEL_CAPABILITIES` and `LINGXI_DISABLE_FAST_MODE` inputs to production
native admission. The [Fast environment oracle](../../scripts/tests/fast_environment_287_oracle.mjs)
executes **1,176** actual source-pinned `To/Ry/ny/N8r/Le` cases with supplied
resolved identities. Global typed disable precedes ordered per-model capability
overrides. The main adapter honors its explicitly selected profile, including
when direct and compatible routes share the same picker label and wire model.

Fast admission is captured before SDK validation and credential work, then reused
by the final body/header guard. The SDK's current `RequestOptions.fast_capability`
and selected codec context carry the request-local observation without changing
the shared client/catalog. An unknown observation retains the catalog; an
observation cannot invent a wire mapping, and it is not applied to a failover
connection. All transport and encoding remain in `llm-client`.

Transport regressions compare all source cases through streaming and non-streaming
requests, actual Bedrock and compatible/direct routes, plus the main session
adapter's toggle/profile combinations. A physical retry changes the environment
between dispatches; the next preparation drops both speed and beta, including an
extra-body attempt to restore speed. Org/account admission, served capability
acquisition, startup env snapshot lifetime and initialize-model projection remain
open. Mods remain deferred and the full alignment goal stays active.

The first complete runtime sweep exposed a pause durability race: a late
connection-scoped AskUserQuestion notification could overwrite a durable
`PausedRecoverable` checkpoint with `WaitingForUser`. Live observer transitions
now compare the checkpoint under the same store lock as explicit pause/resume;
quiescing turn ownership is excluded from durable attribution. Deterministic
store/listener regressions preserve pause metadata while still delivering the
workflow question, and explicit resume re-enables live transitions. The original
failure log and focused/full repair results are retained with this wave.
Current validation passed **4,183** tests, including **398** SDK regressions,
**1,389** runtime tests and **5** production prompt byte locks, plus **32** checks.
The runtime's **1** manual catalog-printing test remains ignored. All four feature
combinations, strict Harness/SDK Clippy and all ten gates passed; the runtime
repair's affected checks were repeated against its final sources. Evidence and
**20** source hashes are recorded in `fast_environment_current_validation`.

The current baked-capability wave replaces the older core model table with all
**21** current native arrays, preserving every capability string and empty row.
The [capability oracle](../../scripts/tests/model_capabilities_287_oracle.mjs)
executes the actual baked catalog, **126** Ry/ny fast gate cases and **126**
Yr standard-prompt gate cases. Sonnet 5.5 hits both the capability and classic-name
branches, so capability-first ordering is now an observable discriminator. Opus 4.7 is
absent from the current fast list; Opus 5.5 is present, matching the existing SDK
preset. Sonnet/Opus 5.5 and Fable 5 now have their current baked prompt/capability
facts. Standard provider wrappers normalize the current identities; non-Claude
models retain their full multi-provider prompt family.

The adapter, native request preparation, final first-party body enforcement and
client model-details projection now share the current fast predicate. Eight new
SDK transport captures cover Opus 4.7 versus 5.5, streaming versus non-streaming,
and first-party versus compatible routes, including body speed and fast beta.
The pure predicate also accepts supplied environment/global/served facts; current
producer acceptance covers the baked branch with those acquisition inputs absent.
Remote capabilities, native To account admission and full initialize-model/auto-mode
semantics remain open. No SDK edit was required: all communication stays in the SDK.

The child fallback audit identified a required distinction. In the actual native
refusal branch, a local subagent retry changes the serving model and keeps its
original tool context; a normal main-thread retry can also change the session and
tool options. The [refusal-scope oracle](../../scripts/tests/child_refusal_scope_287_oracle.mjs)
executes **4** actual swap cases. A decoded-stream runner regression captures the
fallback request and subsequent tool model, preserving the native local behavior.
Changing every child tool model after a refusal would diverge from the reference.
Availability, consent and explicit live-switch transitions remain separate work.
Current validation passed **6,386** component library tests, **23** integrations,
**5** production prompt byte locks and **29** checks, including all four feature
combinations, strict workspace Clippy, all ten gates and the LLM communication
boundary. The runtime's **1** manual catalog-printing test remains ignored.
Evidence and **12** unchanged source hashes are recorded in
`baked_capability_current_validation`. Its final Core/Tool API rerun includes the
new Sonnet 5.5 branch-order regressions and replaces their earlier binary counts.

Complete prompt-section content, served identities and capability/cache acquisition,
full initialize/auto/fast-account acceptance, current model-switch lifetimes and
other release inventory gaps remain open. Mods remain deferred; the full alignment
goal stays active.

The current loop-model wave connects native AM/ny mitigation to production
bundled prompt context and actual lone-wakeup turn completion. The
[model-gate oracle](../../scripts/tests/loop_model_287_oracle.mjs) executes **912**
actual source-pinned cases, including ordered global/per-model capability
overrides, prefix selectors, ECMAScript whitespace, served-true observations,
baked Fable 5/5.1 and Mythos 5.1 mitigation, and Mythos 5 fallback. Explicit
negative overrides precede all fallback paths. The branded override is
`LINGXI_MODEL_CAPABILITIES`; no older spelling is read as a compatibility alias.
Served observations are supplied in these helper cases; their acquisition
remains open. The subsequent capability wave above refreshes the baked core table.

`BundledPromptContext` is the current checked expansion contract, carrying project
root, cwd, preload state and the owning model, without an old positional overload.
The slash dispatcher obtains a live selected-route identity from the owning
orchestrator and preserves that provider when cloned. Stateless templates do not
query a model route. The Skill tool carries its dispatching model; desktop and
mobile preloads receive the child's resolved model from the runner. They preserve
preload behavior and propagate read errors before a model call. The current
preload trait likewise requires the model argument without a forwarding overload.

Prompt generation and actual wakeup turn completion now use the same pure gate;
main slash/turn consumers also share the selected-route identity lookup. Regression
captures include live model switching, shared dispatcher cloning, child model
propagation and native Fable 5 turn termination through both existing drivers.
A mock selected route separates host display labels from its canonical Fable
identity, verifying that the main prompt context and turn boundary agree.
Existing 240-case loop template and 744-case effort command fixtures are retained.
Current validation passed **4,821** component library tests, **6** boundary
integrations and **27** checks: all four feature combinations, strict workspace
Clippy, all ten gates, communication-boundary checks and current source fixtures.
The runtime's **1** manual catalog-printing test remains ignored. Evidence and
23 source hashes are recorded in `loop_model_current_validation`. The final
boundary fixture supplies actual streaming events and verifies both new scenarios
through both drivers; its earlier missing-stream mock failure is preserved in
that record.

Native served/catalog identity overrides, remote capability acquisition,
availability/consent/live-switch child identity, PR stewardship/cloud prompt
splices and full tick/resume behavior remain open. The later refusal-scope audit
above confirms that a local refusal retains the initial child tool identity. This closes the supplied native model gate
and current selected-model/preload wiring, rather than complete loop acceptance.
Mods remain deferred to the other branch; the full alignment goal stays active.

The current local-command wave binds `/effort` to an authoritative selected-route
snapshot without authentication or transport. The [command oracle](../../scripts/tests/effort_command_287_oracle.mjs)
executes **744** actual source-pinned helper/wrapper cases for display text,
ECMAScript whitespace and number display, capped selections, environment
precedence, session rollback and native per-model write admission. A capped
selection, `max`, or a noninteractive command stays session-only. Clearing effort
retains the distinction between inherited and explicitly automatic selection;
failed settings writes restore the original selection. Native per-model effort
and provider-neutral `reasoning.defaultSelection` no longer mirror each other.
All model communication remains in `llm-client`.

Ultracode now has an independent session switch: enabling it preserves the current
effort and writes no effort default; changing effort preserves the switch.
Its command gate uses current selected-model xhigh support and dynamic workflows.
Explicit orchestrator configuration supplies its initial state. This does not
close native startup/settings acquisition, resume/child behavior, or the complete
current reminder content and cadence.

Enabling the command library's builtin feature exposed old `/loop` byte fixtures
that default-feature tests did not execute. The prompt now uses current 2.1.287
status ordering, brief-mode Reply instructions, stop-outcome visibility and
bounded-monitor wording. The [loop oracle](../../scripts/tests/loop_command_287_oracle.mjs)
executes **240** source-pinned combinations with explicit pre-arm/brief inputs.
Unreachable older cron-only dispatch branches and their tests are removed.
The template oracle supplies the pre-arm model result explicitly. The loop-model
wave above adds current model-gate/preload wiring; served acquisition, PR
stewardship/cloud splices and full dispatch/tick acceptance remain open. Expansions
with genuinely absent model context retain the native absent-model branch.

Native settings `hn` merge/atomicity and outer UTF-16/numeric serialization,
interactive picker UI, remote command transport, telemetry/warning experiments,
account/default cache acquisition and live providers remain open. Current validation
passed **5,333** component library tests, **54** integrations and **26** checks,
including all four feature combinations, strict workspace Clippy, all ten gates,
communication-boundary checks and the source-pinned fixtures. The runtime's **1**
manual catalog-printing test remains ignored. The record and 25 source hashes are
in `effort_command_current_validation`; earlier catalog command-library results
used default features and did not cover builtin handlers. Three vanished effort
namespace/path baseline keys were removed after confirming the source cleanup;
the remaining brand baseline entries and comments were preserved.
Mods remain deferred to their other branch. The full alignment goal stays active.

The current beta/count wave uses the actual `av/lr` chronological comparison,
correcting earlier helper fixtures that substituted substring matching. Opus 4.1
supports automatic structured output; Opus/Sonnet 4.0 and Claude 3 do not. Sonnet 5
is outside the older-model list for mid-conversation system. Foundry has distinct
base beta gates from first-party Anthropic; Vertex/Bedrock no longer inherit
first-party-only context management and prompt scope. Supported main requests
append the effort beta even without a body effort value; explicit main effort
suppresses that automatic insertion, and side queries retain their own admission.
The [model beta oracle](../../scripts/tests/betas_287_oracle.mjs) executes
**1,584** source-pinned cases with native built-in canonicalization, typed booleans,
provider/model gates, and default/explicit `YMe` insertion. Served-catalog overrides,
HIPAA taint, beta-denial learning, subscribers and `awn` per-turn features are
supplied absent state; complete header assembly remains open.

The selected-model catalog wave connects SDK effort facts to native preparation
and model listing controls. The [catalog oracle](../../scripts/tests/effort_catalog_287_oracle.mjs)
executes **1,296** actual jw/J/pQ/xXe cases for explicit support, max/xhigh
observations, empty/runtime level lists and unknown fallback. Native builtin
fallback and identity are supplied; bkn/Dbe acquisition remains outside this
oracle. Existing SDK Models API observations pass through configuration merge
and then native request preparation. Explicit unsupported effort removes body
and beta; supported per-level facts can replace builtin max/xhigh restrictions.
SDK/catalog defaults enter the lowest-priority native default slot using its
five-level vocabulary, while host-supplied values keep priority. An additional
21-row source-pinned baked catalog inventory supplies defaults when SDK facts
are absent. Opus 4.7 defaults to xhigh; Opus 5.5 and Sonnet 5.5 default to medium.
SDK Anthropic presets now carry the current Opus 4.7 default and Opus 5.5
max/xhigh levels from that native table. Host display-label changes retain
unique wire-model facts; duplicate wire rows require their exact display identity.

SDK reasoning targets now require selected model features and the connection
inference contract. Empty lists and per-level denial change selector choices;
wire restrictions prevent incomplete enabled/budget selections. The historical
Opus 4.5 host-only exception is removed. Eleven added mock directory-to-request
captures exercise these facts, defaults versus carried/primary/host input, empty
versus absent lists and invalid native default vocabulary. This uses the existing
SDK directory pipeline; no Models API default or ceiling fields are invented.
Current validation is recorded in `effort_catalog_current_validation`: **371** SDK tests, **4035** Harness component library tests, the isolated transport integration, all four feature combinations, strict workspace and SDK Clippy, all ten gates and the current native helper fixtures passed. **1** manual catalog-printing runtime test remains ignored.

Native served-model overrides, account/default-cache acquisition and complete
current session/live-provider acceptance remain open. The local-command wave above
adds command display and per-model write admission; full native settings IO and
picker acceptance remain open. Mods remain deferred to their other branch.

The current settings-effort table wave ports native U6r/Q/ve/ee/he/kd,
dBe/Se/$6r/ye and wu/qj. The [table oracle](../../scripts/tests/effort_table_287_oracle.mjs)
executes **1,158** source-pinned table cases, each with model recognition and
multiple session selections. Canonical aliases and managed override eligibility
are supplied; the source functions determine source priority, path deduplication,
canonical-entry preference, user-root admission and first-start exclusions.
Model settings now preserve JSON property order, including native integer-key
ordering during alias resolution, and discard Object.prototype names before
validating entries. Original policy tiers remain distinct for K caps but become
one merged policy slot for Q defaults. A present undefined entry suppresses
inheritance and remains distinct from a missing entry.

`SessionEffort::{Inherit, Default, Level}` is the current orchestrator setter
contract, without an Option-value forwarding overload. Native service boot
captures the admitted default layers; physical calls refresh caps and pin the
session selection before authentication. Main requests inherit the selected
model's boot default when no explicit parameter is supplied. Explicit automatic
skips that inheritance; side queries without hook effort never adopt it. Restoring
missing effort selects Inherit. Desktop ordinary/headless and mobile launch
options read first-start flags only from their admitted config home. The generic
reasoning loader no longer interprets native effortLevel as an old Harness
fallback; current native user-root consumption remains in Q/ee.

Nineteen additional SDK transport captures cover old/new model identities,
per-model defaults, explicit automatic/level/numeric selection, side isolation,
boot versus live edits, numeric and auto environment behavior, first-start pins
and fresh-session acquisition. Earlier cap and transport checks remain. Current
validation is recorded in `effort_table_current_validation`: **4,780** current
component library tests, **81** integrations, all four feature combinations,
strict workspace Clippy, all ten gates and source-pinned fixture checks passed.
The final focused request test was repeated after the extra auto/cap discriminator;
the only ignored runtime test is the manual catalog-printing helper. Served Rt/Be and
catalog/default/cap acquisition, command display/per-model persistence, complete
resume/scalar/native child acceptance and live providers remain open. Mods remain
deferred; this is another required part of the full active alignment goal.

The current settings-cap wave preserves original admitted effort layers in
`EffectiveSettings::effort_layers` and ports native K(e)'s minimum across them.
A matching per-model cap replaces the top-level cap inside that source; per-model
`max` exempts that source, while another source can still impose a lower cap.
Invalid effort values follow the native enum catch(undefined) behavior without
losing unrelated settings. The [settings-cap oracle](../../scripts/tests/effort_settings_287_oracle.mjs)
executes **1,512** source-pinned K/b/qSe/Ct cases with canonical identity results
supplied explicitly. Desktop ordinary/headless and mobile services sample the
host's admitted sources before every physical preparation, retaining configured
paths, desktop restriction/source gates, flags and managed tiers. An explicit
empty source never rediscovers ambient files. Nine added SDK transport captures
exercise earlier caps under later max, picker aliases, prefix nonmatches, live
file edits, auto/numeric behavior and authoritative empty sources, including
streaming requests and native BOM versus NEL key trimming. Two additional Foundry
captures retain an explicit underlying model identity through host projection, use it for effort support and cap checks,
and leave the deployment name on the wire. This closes the selected cap fold and
host acquisition;
complete native Rt/Be served identity and alias state, remote catalog
defaults/caps, carry lifetime, command consumers and
live-provider acceptance remain open. Validation is recorded separately in
`effort_settings_current_validation`: **4,557** current component library
tests and **81** integrations passed, along with all four feature
combinations, strict workspace Clippy, all gates and source-pinned fixture checks.
The runtime's one ignored item is a manual catalog-printing helper. A pre-existing
context-collapse environment race found in parallel recovery tests is now guarded
by a shared async test mutex; normal parallel library verification passed after
the repair. The complete alignment goal remains active.

The current effort-resolution wave executes the actual iF/$9/jb/T/l_n/we/
S2t/b3/aF functions in **2,092** source-pinned cases. Settings caps, applicable
provider caps, defaults, carried values and model support are supplied host
state; the functions themselves determine precedence, numeric parsing,
auto/unset clearing, max/xhigh downgrade and the side disabled-thinking ceiling.
Core now hosts that pure resolver, and each native service preparation pins its
selected value before SDK validation/authentication. Native default main high
is emitted in the SDK wire policy, while side queries without hook effort do
not inherit it. Explicit main/side extra behavior remains in the SDK. The
branded environment is LINGXI_EFFORT_LEVEL with current native value semantics.
Twelve added dispatch cases and two authentication-mutation captures check
selected body/beta behavior and fresh versus existing preparations. Previous
exact effort strings cannot override a replacement selection. Settings-cap acquisition is covered by the later settings-cap wave above.
Catalog acquisition, dynamic capability overrides, carry lifetime and command
consumers remain open; this is not complete native effort acceptance. Current
validation passed **3,461 library tests**, **152 integrations** and **24 checks**,
including all four feature combinations, workspace/SDK Clippy, all ten gates
and native fixtures. Final no-default/core/transport verification was repeated
after the equivalent branded constant and lifetime cleanup. Branding now has
its complete namespace inventory; 14 corresponding G6 baseline rows were
removed without changing other rows/comments. See
`effort_resolution_current_validation` for the evidence and remaining scope.

The following effort body wave ports native YMe deletion into the SDK merge:
unsupported main effort is removed even when explicit, while side extra config
is spread after automatic pruning. Automatic non-string values are omitted,
and discarded effort cannot leave stale UTF-16 sidecars. The wire oracle now
covers **1,764** exact body cases with supported/unsupported capability and
scalar/array/object/explicit-presence inputs; its **600** model/env probes remain.
The host supplies capability for the selected route, preserving custom gateway
wire inputs. The earlier body-only checkpoint did not resolve jb/default/cap state;
see the current resolver section above for the subsequent acceptance and gaps.
this does not close the complete effort feature. Current effort-body validation
passed **2,715 library tests**, **152 integrations**, all four feature combinations,
strict workspace/SDK Clippy and all ten gates; the source-pinned fixtures and
communication boundary checks also pass. Logs and pending `jb`/cap/default state
are recorded in `effort_body_current_validation` and superseded where stated
by `effort_resolution_current_validation`.

The SDK now prepares countTokens at `/v1/messages/count_tokens?beta=true`,
extracts virtual beta/profile/workspace parameters, and appends
`token-counting-2024-11-01` after caller betas. String betas spread by Unicode code
point; arrays preserve native join behavior, and noniterable values fail before
transport. The [count oracle](../../scripts/tests/count_tokens_287_oracle.mjs)
executes **60** actual native SDK/px/header cases. SDK transport tests check the
final URL/headers/body, and the Harness counter passes model betas through this
SDK normalization before signing. Native CLI count calls, options-header
precedence, cloud counters and live-provider acceptance remain separate work.
Current validation passed **2,714 library tests** and **152 integrations**,
plus all four feature combinations, strict workspace/SDK Clippy, all ten gates,
format/boundary checks and the source-pinned fixture checks. The final equivalent
Clippy cleanup passed **21** additional model-beta regressions. These are scoped
results; the full alignment goal remains active. See
[validation-2.1.287.json](validation-2.1.287.json) for current logs and limitations.
Mods are deferred to the other branch as requested.

## Native reference

- Official release: https://github.com/anthropics/claude-code/releases/tag/v2.1.287
- Published: 2026-10-01T18:00:22Z.
- Official download manifest: https://downloads.claude.ai/claude-code-releases/2.1.287/manifest.json
- Isolated darwin-arm64 reference: `/tmp/harness-claude-code-2.1.287-darwin-arm64`.
- Manifest-verified SHA-256: `6eab8333fe2121553100d8f40bfada384a3e989b94f947e18ba6677a6fcb41ea`.
- Binary size: 227,827,120 bytes. Build date: 2026-10-01T16:26:16Z.
- Executing the isolated binary with `--version` returned `2.1.287 (Claude Code)`.
- Native commit: `3c446a1b98aceb99a6cdee0f84a8bea42f4a8937`.
- Source extraction: 2,175 chunks in `/tmp/claude-code-oracle-2.1.287`, generated
  by [`extract_oracle_287.py`](../../scripts/tests/extract_oracle_287.py).

The [106-item release inventory](claude-code-2.1.287-inventory.json) pins every
release entry by ordinal, source line and text digest. The complete downloaded
notes remain at `/tmp/harness-claude-2.1.287-release-notes.md`; they are reference
material, not shipping runtime data. Unreviewed entries are not parity claims.

## Current scoped implementation and validation

The main non-streaming API now accepts one required owned request. Obsolete
forwarding methods and ignored subscription flags are removed. Main, scheduled
and prompt-hook policy remain explicit. Context-hint offers are unwrapped once,
and per-request beta activation is independent of an offered body. Output limits,
ordered fallback, overload seeds and live hint state accumulate independently
and survive hint rejection, collapse, reactive compaction and streaming fallback.
Query-source dispatch classification uses the current native categories, with
v2d taking precedence. Main, scheduled and hook body policy remains independent
of that header classification. LLM Runtime passed **1,049/1,049**; the adapter's
actual sealed HTTP, seed, schema and combined-option regressions passed in
Orchestrator's **1,313/1,313** full run.

Dispatch recovery now follows the 2.1.287 `hYe/yYe` closure: a carried
`v2s`/`v2d` request failing with a 5xx or connection error re-sends `v2p`;
first-party headerless 503 responses also enter that path. A headerless
`x-should-retry: false` decline re-sends immediately; other headerless 503s
wait the native rounded 500–625ms jitter. The latch belongs to one query,
including its physical retries, and resets for the next query. Native timeout
inheritance is included; user aborts are excluded. Pre-event body connection
failures re-send `v2p`, while a forwarded event suppresses that body branch.
The dispatch record tracks the host's own injected value, separately from
custom provider headers. The SDK still owns sealing, signing and transport;
non-Anthropic/custom-gateway routes and hosted execution replay safety retain
their multi-provider behavior. The current hash-pinned
[`dispatch oracle`](../../scripts/tests/dispatch_recovery_287_oracle.mjs)
executes **208 failure cases**, **16 body eligibility cases** and **6 delay
samples**. Actual SDK traffic checks cover **48 HTTP/recovery/reset cases**,
**8 repeated connection/timeout/body failures**, the forwarded-event boundary,
**8 other-provider/gateway cases**, and registered settlement ordering. This is scoped recovery evidence; native side-query 408/409/429
client-budget semantics and post-event stream recovery remain separate gaps.

One model/count draft owns one exact-profile credential snapshot. Public
`prepare`/`prepare_at`, WebSocket prewarm, handshake, final sealing and 426 HTTP
fallback share credential material; each final method/body/time is signed
again. A fresh draft observes an account change and rebinds the socket, while
the same account reuses its connection. File services retain live credentials.
The ten focused snapshot regressions exercise the real SDK paths; client authentication,
failover, cost and stream integration binaries passed **17/6/16/19**; the
additional authentication strategy binary passed **6**.

Partial frames now close before retry backoff, non-streaming dispatch or partial
finalization. An unfinished text/thinking block gets its raw block-stop before
message-stop; unfinished client/server/MCP tools never receive a synthetic
executor stop. A complete final delta survives clean EOF or eligible connection,
watchdog/server errors without an incomplete notice; subsequent block events
invalidate completion. The exact native helpers passed **14 close**, **24
completion** and **16 error-eligibility** cases, replayed by Rust. Full caller
tests retain native empty-response nudging and interactive versus headless
truncation policy. Abort/retraction and all native outer error-policy branches
are outside this acceptance claim.

MCP connection generations freeze the exact current elicitation authority.
Eligible ordinary transports default to form and URL; explicit
`bareElicitationCapability` selects the native bare legacy projection without
downgrading modern metadata. Roots advertise `listChanged:true`; the existing
add-directory path sends the actual roots-change notification. The pinned
native capability helper passed **558 cases**; **540** boolean/default cases
are replayed by Rust, while the 18 polymorphic feature-value probes remain
native-helper evidence only. MCP and POSIX full libraries passed **668/163**,
mock protocol integration passed **26**, and actual HTTP integration passed
**4**, locking the full capability object and field order. Core passed **743**.
The listener fixture now registers its actual generation-bound client; the new
regression rejects borrowing another generation's capability authority.

Telemetry emits `prompt_text` as the same processed value as `prompt`, retaining
the existing opt-in, redaction and cap policy for both fields. The full library
passed **154/154**. Eight ad hoc executions of the exact native `pe/me/c$t`
helper confirm the copy relationship; they are not a persisted oracle or full
native exporter acceptance. The native helper is in `src_183521709.js`, byte
range `[9166,9312)`, SHA-256
`7b0e5eb74012cd9ba63e560e59da3ada64b032a4c719e7565b8b328b8390532b`.

Main-loop nested-memory acquisition now consumes a dedicated insertion-ordered
trigger queue shared with the actual tool invocation context, rather than
scanning ReadState's model-visible LRU. Text/notebook reads enqueue after
successful acquisition; images enqueue after successful decode. Failed reads,
ordinary unchanged-read stubs, seeds, writes and restored cache entries do not
invent triggers. A queued path survives cache eviction, and a consumed path
cannot discover a file which appears later without a new producer event.
The old split conditional-rule acquisition entrypoint is removed; one
per-trigger consumer retains the durable current attachment path. Independent
child tool contexts do not enqueue the main loop even when their current file
cache is shared. The fixed native `h7t` oracle executes **15 consumption cases**
plus **4 enqueue snippets**, including pending transfer, disable, live iteration
and failure retention. Full Tool API, image/PDF-enabled File tools and
Orchestrator libraries passed **252/407/1,313**, and the affected compaction,
memory and resume integration binaries passed **79**; the isolated actual
Hook/Main dispatch binary passed **1** with the latest `v2p` recovery byte. Actual Read regressions
cover late file creation, genuine LRU eviction, cache-only entries, unchanged
and failed reads, isolated child contexts, text/notebook production and image
decode success/failure. These control primitives do not prove the complete
worker attachment pipeline, internal `readFor`, input retraction or the
flag-gated coordinator propagation producers; those integrations remain open.

Nested-memory attachments persist each file's untrimmed acquired body, raw-content
metadata, import parent and UUID. Visible post-boundary bodies drive dedup;
ordinary read-state seeding remains a separate normalized view. Native eager
filtering applies to each rule-directory acquisition result, retaining eligible
unconditional imports and named files with globs. BOM detection and frontmatter
ECMAScript whitespace now match the executed native cases. Memory passed
**197/197**; the native eager oracle passed **12 acquisition/render + 11 hooks**,
and nested-memory history/dedup passed **9 cases**.

Compaction snapshots and clears the entire read cache, including nested seeds,
then restores eligible folder/import files with native Read normalization and
token budgeting. Each file owns one durable raw attachment UUID and both Read
reminder blocks. Cold/hot replay projects that same payload and preserves its
parent chain; compact references use the same current attachment path. The native
restore oracle passed **8 cases**, and the file/reference renderer **6**. The
eighth restore case controls fresh resume state; it does not prove the native
resume initializer. The native five eager entrypoints, plan, retained actual
Read calls and direct artifact paths are excluded. The obsolete reader and its
20k-character/60k-byte clamps were removed; bounded reads now feed the native
post-read token decision instead of silently truncating content.

Both the shared boundary predicate and actual API-history normalization now
recognize only typed `system/compact_boundary`. The old ordinary-system-text
sentinel fallback is deleted, without a migration parser. Current tool-reference
normalization uses the native eight-entry table; four obsolete Output mappings
and the Harness-only `subagent` query-source alias are removed. The native name
oracle passes **19 cases**, replayed through actual API-history normalization.
The exact native
boundary oracle passed **6 cases**. Compaction and Session full libraries passed
**276/322**. The eight affected Orchestrator integration binaries passed **80
tests**, including strict raw UUID, payload and complete cold-resume parent
checks. Desktop/mobile/UniFFI Runtime passed **1,386** (one ignored); Agent,
Tasks, Fusion and Sidequery passed **595/534/264/60**. Runtime background-writer,
real pool-starvation and SDK-surface integration passed **5/3/1**. Subprocess
replays are included in their owning suite's total, not counted twice.

## Final validation checkpoint

The [machine-readable validation record](validation-2.1.287.json) records
**8,383 passing library tests** across 16 named packages, one explicitly ignored
Runtime test, and **183 integration tests** across the affected binaries. These
are scoped regression results, not an assertion that the full native surface
is implemented. It retains each log and excludes filtered subprocess replays
from totals. The queue wave re-ran Tool API, image/PDF-enabled File tools and Orchestrator
libraries (**252/407/1,313**) plus **79** compaction/memory/resume integrations and **1** isolated Hook/Main
dispatch integration.
The preceding dispatch wave ran LLM Runtime and Orchestrator (**1,049/1,308**)
and **64** SDK integrations. Other unchanged-package totals retain their cited
scoped passes; they are not freshly re-run whole-workspace acceptance.

All four host build profiles passed with locked offline dependencies: no default
features; independent desktop; independent mobile/UniFFI; and workspace all
features/all targets. Unified Clippy passed for **19 affected packages**, all
targets, with `-D warnings`. Formatting, diff checking and the model-endpoint
boundary check passed. The SDK submodule remained clean at
`0c6a907d897a54e656700c00cf335d91b10dca0b`.

All **10** discovered repository gates executed and returned zero. The independent
trigger audit re-executed them and matched their real results. Brand baseline
changes are limited to the two native fixture path-conversion literals in one
test and one removed stale test entry; no production brand exception was added.

The official latest release was checked again at this checkpoint and remains
2.1.287. Build checks run on the current macOS host; they are not physical
device, Linux/Windows binary, packaged client or live-provider acceptance.

Mod functionality is being implemented on another branch and is deferred from
this wave at the user's request.

The experimental-beta switch is read through the actual native `Le` →
`D.bool()` → `ZZe` environment getter. Only `1`, `true`, `yes` and `on`
(case-insensitive, ECMAScript-trimmed) disable automatic schemas. `0`, `false`,
unknown words and whitespace do not. The preceding checkpoint's raw-string
`rK` stub omitted that getter; its nonempty-string conclusion and corresponding
assertions are superseded by the current getter-executing oracle and regressions.

Automatic schema admission now includes the actual `GF/zF/own` route gate and
chronological exclusion of models older than Opus 4.1. Ordinary Bedrock/Vertex paths omit automatic formats;
Anthropic Messages and Foundry can carry them. Main requests preserve an
explicit extra-body format before validation and encoding; side queries admit
the automatic format first and let the explicit extra body overwrite the final
config. The original typed schema remains available to another-provider fallback.

The SDK now owns distinct current main/side-query merging. Main `YMe/YLo/ZLo`
preserve explicit config fields; side queries replace the complete config with
an extra-body value, including null and non-object values. ECMAScript numeric
key order, collision positions and UTF-16 string spreading are retained, with
exact string sidecars updated or cleared alongside replaced fields. Structured
output beta tokens are added for admitted automatic formats, independently of
private first-party routing headers. Request body kind is independent of dispatch
category. Extra-body snapshots belong to a physical preparation, before sealing
and signing. SDK source changes are local and have not been committed/published;
its Git revision pin remains unchanged pending that separate delivery operation.

The hash-pinned [structured-output oracle](../../scripts/tests/structured_output_287_oracle.mjs)
now executes the actual environment parser/getter in its **256 cases**. The
[wire-policy oracle](../../scripts/tests/output_config_wire_287_oracle.mjs)
executes native config insertion and the final main object template, yielding
**1,764 exact-byte cases** plus **600 actual provider/model/env probes**. All 1,764
policy byte cases pass in the SDK, including astral UTF-16 spreading. Effort
support and dynamic Foundry capability permission remain controlled oracle inputs.
The isolated host regression now covers **600** additional real SDK
stream/nonstream preparations for four mapped protocols, five models and 15 env
values, alongside the preceding eight-env preparation/transport/fallback coverage.
It also verifies enabled/explicit main and side configs, one structured beta token,
UTF-16 bytes at the real SDK transport, and a host-driven Anthropic→OpenAI fallback.
The Hook regression now checks both accepted true spellings and `0`/`false`/unknown
false spellings. These captured SDK requests are not live-provider acceptance.

The earlier config checkpoint LLM Runtime/Orchestrator/Sidequery runs passed
**1,049/1,313/60**, with **81** current Harness integrations. SDK all-feature
library regression passed **279**, and its four selected Anthropic/prepared
integration binaries passed **62**, with **8** additional WebSocket transport
regressions. All four Harness
feature combinations and strict all-target/all-feature Clippy passed for both
the full workspace and the SDK. All ten gates, the execution-trigger audit,
format checks and LLM communication boundary checks passed. SDK pricing's
optional-rate loop was simplified equivalently for Clippy; WebSocket fixtures
now read complete bounded handshake headers. Two callback-only test lint
exceptions document Tungstenite's fixed external error type.
The preceding Rust 1.94 permission-test lint repair remains equivalent; its
filtered run does not establish full permission acceptance. Complete native
API/CLI, live-provider and device acceptance remains open.

## Extra body and Messages SDK parameters

The [extra-body oracle](../../scripts/tests/extra_body_287_oracle.mjs) executes
hash-pinned native `hV`, `gt/se/Do`, `N`, `K_t` and instrumented `JSON.stringify`.
Its **128** sanitation cases cover type preservation, nested-array string
coercion, Unicode trim/lowercase, AFK removal, metadata identity cleanup and
string bytes; **10** raw-parser cases cover invalid/nonobject inputs and one
leading BOM. Logging, caching and measurement are supplied; HIPAA is false.
This is scoped value/string acceptance, not whole HTTP JSON equality.

The [SDK parameter oracle](../../scripts/tests/message_parameters_287_oracle.mjs)
executes native beta Messages `create`, `ve/MH`, and the real array predicate.
Its **54** cases cover null/default-header behavior, booleans/numbers, empty and
mixed arrays, header trimming, invalid control/Unicode values and object string
coercion. The helper uses the Node Headers builtin; embedded Bun/fetch header
validation is not independently accepted. The SDK moves `betas`, `user_profile_id`, `workspace_id` out of the
JSON body on Messages/Foundry. Explicit extra `betas` replaces automatic betas;
if AFK removal deletes the key, ordinary automatic beta assembly still runs.
Arrays of ID header values use native comma-space append semantics, while the
beta array uses comma `toString()` semantics.

Preparation evaluates the sanitized extra snapshot before schema/effort gates.
Fallible policy errors propagate before sealing/transport. Captured transport
requests exercise all four Anthropic families for cleanup and both Messages/
Foundry for parameter extraction and null beta overrides. Other providers keep
their own schema and failover behavior. No obsolete API overload or compatibility
forwarder was added; Mod work remains deferred to the other branch.

CountTokens virtual parameters, explicit
request-options header precedence, HIPAA admission, cleanup diagnostics and
arbitrary JavaScript
JSON numbers/UTF-16 parsing remain outside this acceptance. The later JavaScript JSON encoding section supersedes the temporary
Rust shortest-decimal formatter and its selected magnitude-boundary coverage.

## Current SDK output format and JavaScript JSON encoding

The [output-format oracle](../../scripts/tests/output_format_287_oracle.mjs)
executes unmodified native `px` and records **179** exact-byte/error cases.
Truthy format parameters normalize into `output_config.format`; false/null/zero/
empty-string parameters retain their original shape. Existing truthy formats
reject the call. Numeric-index config spread, astral UTF-16 units, config field
positions and format sidecars retain the native semantics.

The [number oracle](../../scripts/tests/json_numbers_287_oracle.mjs) executes
native's instrumented JSON.stringify wrapper with its measurement hook supplied.
Its **4113** IEEE-754 bit fixtures avoid decimal fixture rounding and cover fixed/
scientific notation, negative zero, integer boundaries and deterministic random
finite doubles. Each fixture compares decimal parsing, scalar coercion, JSON
number bytes and nested object/array bytes with numeric key enumeration. Execution
uses Node's ECMAScript builtin, not independently accepted embedded Bun.

The SDK uses [ryu-js](https://github.com/boa-dev/ryu-js) 1.0.3 and enables serde's
`float_roundtrip` parser. Current `JsonEncoding` is required by the exact serializer;
no old two-argument overload remains. The borrowed codec serializer and body-length
counter keep payloads borrowed. Final draft serialization, host byte inspection
and authentication choose the same protocol mode: four Claude protocols use
JavaScript; the other providers retain Serde, including full unsigned integers.
Current request envelopes must declare their mode; old missing-mode envelopes
are rejected. Transport captures test the numeric bytes and `px` conversion in
all four Claude protocols, including rejection before dispatch.

This acceptance concerns JSON encoding and SDK normalization. Arbitrary outer extra JSON still needs exact UTF-16 parsing and diagnostic
behavior. The subsequent identity-string section closes unpaired metadata keys,
numeric overflow and deep identity parsing. Whole native request composition, main/side
base-field order, endpoint/query behavior, SDK request-options precedence and
HIPAA/dynamic capability contexts remain open. Passing helpers and a protocol
serializer do not establish byte equality of an entire native CLI session.

## Actual CLI request capture and current wire layout

[The live request harness](../../scripts/tests/live_requests_287_oracle.py)
executes the SHA-pinned **2.1.287 binary**, with `--bare`, fresh config/work paths,
fake credentials, a loopback-only scripted API and refused proxy CONNECT/unknown
paths. **8** actual CLI scenarios produce **8** captured HTTP calls: baseline,
numeric extras, parameter headers, current output format, pre-dispatch format
conflict, unpaired metadata, schema result and unavailable-Agent recovery.
Normal scenarios finish with success results; format conflict makes no HTTP call.
Raw capture files remain at `/tmp/harness-287-live-requests-v4`, including body
hashes, header lists, exact request bytes, stdout, diagnostics and reports.

The capture proves `/v1/messages?beta=true`, native main field order and identity
cleanup through the entire binary. The `--bare` Agent test is **unavailable-tool
recovery**, not a child-agent execution: its tool-result error is asserted. No
child-agent, full non-bare session, live provider or physical device acceptance
is inferred from these runs.

The [layout oracle](../../scripts/tests/request_layout_287_oracle.mjs) executes
unmodified main `pd` and side `En` object templates. **48** cases vary computed
fields, input order, extra collisions, tail fields and final main stream spread.
SDK policy orders existing base fields before extras, preserves collision
positions, and normalizes direct Messages/Foundry URLs to `beta=true`; streamed
and non-streamed drafts use the final URL before signing. Missing caller defaults
remain outside this field-order acceptance.

The extended **128-case** sanitation corpus adds unpaired keys/leaves, duplicate
keys, root-vs-nested `tk`, `__proto__`, numeric overflow, invalid syntax and **2048**
nested arrays. An arena plus explicit parser/writer stacks avoids dropping deep
valid identities and retains each UTF-16 key. Captured Harness requests check
this cleanup in all four Claude protocols and verify the final query/field order
in the direct transports. Outer extra-body UTF-16 parsing is still open.

## Remaining native gaps

- Full native beta/effort model resolution and overrides, dynamic Foundry
  capability denial/learning, HIPAA context wiring, and the remaining extra-body/
  SDK-parameter semantics listed above. The current config-policy, extra-body and typed
  environment getter acceptance is narrower than the complete native API client.
- Complete worker trigger/attachment lifetime, real internal `readFor`, input
  retraction and coordinator propagation wiring; native cold-resume ReadState
  initialization. Main-loop trigger consumption and actual Read producers
  now have scoped acceptance.
- Complete marked token/import semantics. The additional **20-case** native
  marked oracle proves remaining HTML-container, comment blank-line and
  multi-line code/import differences; only its three frontmatter cases are
  Rust regression acceptance. Installed Bun **1.3.11** helper execution is not
  acceptance of the native binary's embedded Bun **1.4.3**.
- Worktree alias mapping, linked-source metadata and walker partial-result,
  ordering, symlink/error/health semantics.
- Typed Java SDK single bare-rescue behavior and Runtime URL dialog/completion.
- Native side-query dispatch recovery for retryable 408/409/429 responses,
  client retry-budget semantics, body error-code metadata and post-event
  recovery policy; previous version-register gaps still apply.
- Unreviewed release inventory entries and the complete native session surface.

The [2.1.286 register](claude-code-2.1.286.md) records the preceding scoped native
oracles and implemented behavior. Its passing tests remain regression evidence;
they do not independently establish 2.1.287 byte equality. Its unresolved gaps
remain open unless a current source audit closes them. The exported outward
version must not be advanced before the full current acceptance boundary passes.

## Reproduce extraction

```sh
python3 scripts/tests/extract_oracle_287.py /path/to/2.1.287/claude /tmp/new-oracle-287
python3 scripts/tests/extract_oracle_287.py /path/to/2.1.287/claude /tmp/new-oracle-287 --check
```

Complete native sessions, live providers, physical devices and packaged clients
remain unverified. The active sibling app runtime Git pin has not been migrated.
The overall latest-version alignment goal remains incomplete.
