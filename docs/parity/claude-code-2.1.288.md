# Claude Code 2.1.288 alignment

The official latest advanced to 2.1.288 during the final recheck of the completed
2.1.287 environmental OAuth wave. Its 5,076 passing tests and 40 passing checks
remain [2.1.287 evidence](validation-2.1.287.json); they do not establish 2.1.288
parity. Current APIs only, with no old Harness forwarding or compatibility paths.
Brand, multiple LLM providers, Fusion and Create Local App remain authorized
differences. The user added Mods to the same alignment goal starting with the
next wave on 2026-10-03. The specified commits have now been applied by a
three-way integration to the current Harness and Product working trees. Git
HEADs and indexes remain unchanged; conflict resolution and integrated
validation are in progress. The original `codex/claude-mods-parity` worktrees
remain intact. See the [integration record](claude-mods-integration-2026-10-03.md)
and the historical
[Mods handoff](/Users/luolingfeng/lingxi/.worktrees/claude-mods-parity/harness-runtime/docs/parity/claude-mods-handoff-2026-10-03.md),
then read its contract matrix and implementation ledger, reconcile the current
runtime/SDK interfaces, and validate the integrated behavior. Preserve both
worktrees and the Product lockfile. The user supplied the committed baselines:

| Repository | Branch | Worktree | Mod commit |
| --- | --- | --- | --- |
| Harness | `codex/claude-mods-parity` | `/Users/luolingfeng/lingxi/.worktrees/claude-mods-parity/harness-runtime` | `1e8ffd9c697d402fb0d0c142a156b1de56e8e385` |
| Product | `codex/claude-mods-parity` | `/Users/luolingfeng/lingxi/.worktrees/claude-mods-parity/lingxi-app` | `12b811e230c3de2de7f1c9632d58280997e3ca17` |

Both commits resolve locally and were inspected on 2026-10-03. Recheck HEAD,
dirty paths and their merge state before continuing; the handoff's earlier
dirty/no-commit description predates these committed baselines. No new commit,
push, or PR has been created by the working-tree integration.
The handoff records a disabled native Mods rollout; static validation and local
tests cannot establish Mods byte parity. Recheck the latest release and rollout
before the next wave's native byte comparisons. Earlier deferred-Mod statements
below describe historical waves and are superseded by this scope update.

The latest release recheck on 2026-10-03 advanced the goal to
[2.1.289](https://github.com/anthropics/claude-code/releases/tag/v2.1.289).
This file retains version-specific 2.1.288 evidence; it does not prove 2.1.289
or integrated Mods parity. Validation counts below describe their original
frozen source scopes and must not be reused as proof for the integrated tree.

The current server-fallback notice/repair wave uses six GPT-6 Luna workers at
max reasoning. SDK classified July rejection facts are integrated into a
conversation-keyed repair state, with native `a1` captured before extra-body
merge and one strip retry per API query. The protocol gate includes first-party,
Bedrock, Vertex and Foundry. Native loopback probes exercise all four wrappers
with fake credentials; each native process intentionally ends with exit 1 on
continued mock 400 responses. The probe/check command passes; this is not a
successful provider call.

The [notice/consumer oracle](../../scripts/tests/server_fallback_notice_288_oracle.mjs)
executes pinned sHo/text helpers, J seed/merge, Je/Ff/pre, Unicode truncation and
local transcript export. Model labels, QA/traits and some input row envelopes
are supplied; source spans distinguish runtime acquisition from these facts.
The [repair oracle](../../scripts/tests/server_fallback_repair_288_oracle.mjs)
compares **64** cause/retry cases and **24** a1 cases.
The [accounting oracle](../../scripts/tests/server_fallback_accounting_288_oracle.mjs)
proves iteration selection and Mwe call order using synthetic units, with
**5** accounting and **6** model-selection cases;
exact USD/credit accounting remains open.

Main and local notices now queue at acceptance and flush after assistant
persistence or at decline/catch. Both scopes persist typed rows with the
original enqueue timestamp. Same-model copy suppression, category sanitization,
303 query-local counts, hot first-route restoration and cold assistant-model
selection are tested. Notices do not choose a cold model after a crash before
its fallback assistant row. Multi-hop cold provider-profile acquisition remains
limited rather than inventing a restoration marker.

The pump tracks completed row UUIDs independently of provider message ids and
API indices. Every admitted fallback Start reports model/id facts, including
suppressed controller events. A pending J is overwritten by the next visible
nonempty seed, and joins at the next eligible completed text row with literal
concatenation; later connection errors preserve the completed seam. The latest
retained originals determine replacement deletion. Nonvisible controls do not
reset tools or mutate display/stitch state. `supersedesUuids` follows the native
main/sdk predicate. Per-row writer tests cover model/profile/provider id,
ordered supersedes and tool-result associations. Producer/adapter contracts
cover replacement and tool-card discard; product reducers have not been updated
or validated. Live JSONL timing, original-row/tombstone history, parent envelopes,
silent io and the complete output event stream remain open.

The previous selected notice-wave validation passed **8,618** test executions and
**23** checks, with **9** ignored. The report records
**144** selected source hashes and **578** unchanged SDK input hashes
in `server_fallback_notice_current_validation`. All nine selected libraries,
23 client integration targets, SDK full tests, three platform combinations,
workspace checks, strict workspace/SDK Clippy and repository gates pass.
Superseded failing iterations are excluded from these totals. The nine-library
suite precedes only three retained-comment demotions for the UniFFI budget and
an equivalent test-only if/else. Client contracts/downstream checks and the
notice suite/Clippy respectively are rerun; the report retains both cleanup
scopes and five supplemental client source hashes.

Full acquisition, per-iteration billing, child admission/terminal envelopes,
model-selection generations, effort/fast-mode, wrappers/count_tokens,
durability/telemetry and the remaining release inventory also remain open.
The full alignment goal stays active; Mod remains with the other branch.

The completed server-fallback wave connects decoded response controls to host
query/model state. Six GPT-6 Luna workers at max reasoning implemented separate
SDK, projection, executor, pure control, main-query and child-query scopes.
`llm-client` owns decoded wire controls, native content/iteration helpers and
explicit model projection. Default SDK collectors remain passive. The host
supplies lane authority independently of whether NHt adds wire parameters.

The [projection oracle](../../scripts/tests/server_fallback_projection_288_oracle.mjs)
executes actual pinned stream-hop, terminal/interruption and Sbn bodies across
**270** single-hop streams, **56** multi-hop sequences and **216** nonstream cases
with **10** source ranges. SDK tests compare the single-hop and nonstream values;
physical host tests separately cover suppressed events and repeated hops. The
[control oracle](../../scripts/tests/server_fallback_control_288_oracle.mjs)
executes Tle/oHo and Mn/Ucn with **40** effect cases and **6** identity cases over
**5** ranges. Lanes, completed single-block messages, effort and model/policy
facts are supplied; this does not prove automatic native feature acquisition.

[Physical projection tests](../../crates/llm-runtime/tests/server_fallback_projection_288_test.rs)
cover first-party/custom isolation, request IDs, text/replay companions, nontext
discard, pending/error boundaries, usage-only empty targets, malformed controls,
reserved-metadata spoofing, raw provider observation isolation and ordinary
served-model differences without a hop. The live pump also consumes
owned terminal/observation model metadata when a later hop event is suppressed.
[Main-query tests](../../crates/orchestrator/src/server_fallback_288_tests.rs)
cover received-model policy, declared spelling, profile/clear restoration,
multi-hop ownership, request-route snapshots, user model/provider picks and
query-local counts. Batched and live
streaming paths use the same controller; a declined stream cancels work,
retracts its attempt, settles known usage and bypasses partial/nonstream recovery.
[Executor tests](../../crates/orchestrator/src/streaming_loop_server_fallback_tests.rs)
use a start handshake to verify real in-flight cancellation, old result discard,
and fresh user-parented tokens. Completed side effects cannot be rolled back.

Child tests validate a route from admitted response facts, keeping original tool
context while sending subsequent API requests on the local model/profile. They
do not prove ordinary child lane acquisition. Desktop policy snapshots preserve
managed read/parse failure, regular availableModels and resolved-default facts;
provider catalogs never supply organization allowlist authority.

Historical selected validation passed **8,374** tests and
**20** checks with **9** ignored and
**130** source hashes in `server_fallback_current_validation`. Three
platform combinations, workspace checks, strict workspace/SDK Clippy, repository
gates and two prior-scope native CLI probes pass. Those probes use fake
credentials and loopback services; live-provider behavior remains unverified.

Accepted midstream display stitching/tombstones, successful native notices and
persistence/resume restoration remain open. So do complete model/feature/
entitlement and dynamic settings acquisition, default July HTTP-beta rejection,
child admission/terminal envelopes, same-route selection generations,
effort/fast-mode, credit/cost, wrappers,
wire/JSONL bytes, durability/telemetry and the remaining release inventory.
The full alignment goal stays active; Mod remains with the other branch.

The completed fallback-request wave connects native server-lane request production.
The host supplies a resolved model/feature snapshot in
`OrchestratorConfig.server_refusal_fallback`; query facts remain outside model
input. Core applies HVn/zFo and ApiService checks rejection state after binding
the provider profile. `llm-client` owns NHt/Mn, June/July beta order and the final
transport-beta gate. Parameters are generated before the native extra-body
spread, which retains authority to replace `fallbacks` or `model`.

The [native oracle](../../scripts/tests/fallback_request_288_oracle.mjs) executes
actual HVn/zFo, AO/qy/sm/jp, NHt/Mn, Le and the final transport gate across
**1,024** admission cases, **624** requests and **16** environment values over
**8** pinned source ranges. Model availability/category, default eligibility,
feature/preference and query facts are supplied; complete acquisition is open.
Silent arm can latch a beta without appending it, while an already supplied beta
can still admit parameters. Threaded requests do not arm a new lane. Raw request
model equality and both sticky active/rejected bits remain independent.

The [physical API regression](../../crates/llm-runtime/tests/fallback_request_288_test.rs)
performs **41** SDK dispatches covering explicit/default requests, silent and
threaded gates, extra overrides, sticky betas without a lane, rejection, reset
and a custom provider while first-party state is live. Server beta state is
partitioned by provider/profile/protocol to preserve the authorized multi-provider
behavior. [Four main-query calls](../../crates/orchestrator/src/fallback_request_288_tests.rs)
cover streaming and batched query admission, typed disable `0`/`1`, retained
sticky state and clear. These tests supply resolved host facts and return ordinary
successful responses; they do not prove a server-swap acceptance path.

Historical selected validation passed **8,333** tests and **20** checks,
with **9** ignored tests and **101** source hashes in
`fallback_request_current_validation`. Three platform combinations, workspace
checks, strict workspace/SDK Clippy and repository gates pass. Two native CLI
probes revalidate the earlier unarmed control/replay behavior, without claiming
new admitted-request acquisition. Historical totals retain their own scope.

Automatic catalog/served/alias/availability/feature/preference/counter acquisition,
the full jHt/IFo/category resolver and actual server swap acceptance remain open.
Visible local retry/dialog, suppression logging and silent-arm/episode ownership
remain controller work.
So do salvage, tool tombstones, credit/cost ownership, replay-derived server betas,
carried effort/settings, cyber header-lane acquisition, complete wrappers and
wire/JSONL bytes, durability/telemetry/repair sinks and the remaining release
inventory. The refusal header-lane bit remains separate from the new server
request admission. Mod stays with the other branch and the full goal stays active.

The completed fallback-response wave moves Messages control parsing into
`llm-client`. Valid and malformed numeric-index fallback starts stay outside
assistant content and provider replay. Native category validation counts UTF-16
units; iteration counters accept finite nonnegative fractions, arbitrary string
types and empty model strings. The final defined fallback-message model wins.
Observing any of these facts does not enable a lane or change the selected model.

The [native oracle](../../scripts/tests/fallback_response_288_oracle.mjs) executes
actual dependency-free lNe/vle/BHt/dNe/UHt/Cle/HHt/wle/$cn functions: **734** complete
blocks, **960** stream starts and **305** usage cases over **3** pinned source
ranges. It compares parsed JSON values, including fractional and negative control
indices and astral category boundaries; it does not establish complete DTO bytes.

The [host regression](../../crates/llm-runtime/tests/fallback_response_288_test.rs)
performs **8** physical SDK calls across first-party/custom profiles, stream and
nonstream, valid and malformed controls. SDK facts reach host
`llm_client.anthropic_fallback` metadata, with a chunked terminal metadata path.
[Two SDK collector calls](../../deps/llm-client/tests/fallback_response.rs) retain
multiple hops and iterations after completion or interruption, preserving other
native metadata. [Two native CLI probes](../../scripts/tests/fallback_response_live_288.py)
perform a second user query after valid or malformed unarmed controls: the model
stays selected and controls do not enter subsequent request content. They use
fake credentials and isolated loopback services.

Historical selected validation passed **8,319** tests and **17** checks,
with **9** ignored tests and **93** source hashes recorded in
`fallback_response_current_validation`. Three platform combinations, workspace
checks, strict workspace/SDK Clippy, repository gates and both CLI probes pass.
Historical test/check totals below retain their own scope.

Server-lane admission/request production, actual host swap acceptance, salvage,
tool tombstones, credit/cost ownership, carried-effort/settings acquisition and
full wrapper ownership remain open. So do exact nullable/extra notices, complete
wire/JSONL bytes and interruption durability, concrete telemetry/repair sinks,
model beta-cache admission and the remaining release inventory. The host still
supplies lane_enabled=false. Mod stays with the other branch and the full goal
stays active. No old compatibility API was introduced.

The completed refusal-state wave retains native flat `model_refusal_fallback`
notices through persistence and replay. Cold and hot resume recover the live
model-selection latch and cyber header state independently. User model picks
remove the model latch while retaining header state. Clear restores the original
model and the authorized provider profile before releasing session state. A fork
copy neutralizes inherited notices and uses the model before the refusal swap;
the live parent keeps its model and latch. Background handoff retains its existing
session state.

The [state oracle](../../scripts/tests/refusal_state_288_oracle.mjs) executes actual
native pe/vvt/vn/cCe/dCe helpers: **1,944** latch/transition cases and **1,944**
resume cases, including undefined/null/string settings, owned versus stale
latches, case-sensitive wire identity and independent header decisions. Native
`Mn` removes every `[1m]` and `[2m]` tag while preserving case; it remains separate
from fast rejection identity. The SDK matches **272** actual header cases over
**40** captured source ranges, including ASCII origin validation, trailing-space
normalization and retained explicit headers. Header parsing/emission belongs to
`llm-client`; no obsolete compatibility APIs or nested notice fallback were added.

The [host regressions](../../crates/orchestrator/src/refusal_resume_288_tests.rs)
cover **5** physical SDK dispatches across hot/cold resume, a model pick, clear
and a neutralized notice. The local producer writes the flat notice, the real
fork backend receives a neutralized copy and the original model, and clear
restores the original profile. Refusal notices stay outside provider messages.
[Two native CLI probes](../../scripts/tests/refusal_resume_live_288.py) confirm
actual --bare --resume acquisition: a cyber notice sends both refusal headers
on the next query, while a neutralized notice sends neither. They use fake
credentials and an explicitly assumed first-party loopback gateway.

Historical selected validation passed **8,363** tests and **35** checks,
with **9** existing tests ignored and **83** unchanged source hashes recorded in
`refusal_state_current_validation`. Platform combinations, strict workspace/SDK
Clippy, repository gates and **13** native CLI probes pass. Earlier probes retain
their display/named-beta/pF scope; the new pair proves resume headers.

Complete server fallback block/iteration acquisition and swap acceptance remain
open, including salvage, tool tombstones, refusal credit and cost ownership.
Initial/alias/transport/feature acquisition, native anthropicAws/count_tokens
wrapper gates, carried-effort acquisition/application, complete previous settings
and wrapper ownership also remain open. Exact nullable/extra notice fields and
full SDK/JSONL envelopes, concrete telemetry/repair producers, model beta-cache
admission and the remaining release inventory are still incomplete. The host
currently supplies lane_enabled=false; this wave establishes rearmed cyber
headers rather than feature acquisition. Mod stays with the other branch and the
full goal stays active. Earlier waves below retain their historical scope.

The completed fallback-target wave supplied the native **$Fo/P1e** query facts
to IR and fast repair. A matching turn override or configured chain step is a
target only when its normalized identity differs from the user's selected model.
A session latch remains live only while its raw model equals the current raw
main-loop override; normalized equivalence cannot keep a stale latch alive.
The streaming query scope now carries these facts through initial open, PTL
recovery, stream reopen and nonstream fallback. Batched requests use the same
host capture and subagents retain their own selected model and turn override.
The execution context remains outside provider input; protocol parsing stays
in `llm-client`, with the prior target-string API removed rather than forwarded.

An accepted user model pick clears local target ownership without rearming the
refusal episode. Switching away and back cannot resurrect an old target. The
[native target oracle](../../scripts/tests/fallback_target_288_oracle.mjs) runs
**4,500** actual $Fo/P1e cases plus the native model-selection class and `oae`
model-pick removal. It captures **37** source ranges and supplies absent served
aliases, transport parsing and model overrides to the native bare-wire identity
branch. Query and latch facts are explicit inputs; full server/header/restore
acquisition is not established by this corpus.

The expanded [physical SDK regression](../../crates/llm-runtime/tests/fast_refusal_288_test.rs)
performs **49** stream/nonstream dispatches, including user-model equality,
configured chain ownership and raw live/stale latches. A new
[configured-chain regression](../../crates/orchestrator/tests/model_fallback_288_test.rs)
performs **3** physical calls: server failure on the primary, fast refusal on the
fallback and successful standard-speed retry. It retains display updates and
original messages with zero ordinary retries and a decline header. The
[batched lifecycle regression](../../crates/orchestrator/src/conversation/tests/refusal_fallback_tests.rs)
observes **3** host API calls across the accepted latch, a user switch away and
selection of the former target again; the routing latch remains spent.

Historical fallback-target validation passed **8,357** tests and **32** checks, with **9**
existing tests ignored and **71** unchanged source hashes recorded in
`fallback_target_current_validation`. Platform feature combinations, strict
workspace/SDK Clippy, repository gates and the prior **11** native CLI probes
pass. Those CLI probes use fake credentials and isolated loopback services and
revalidate prior display, named-beta and pF behavior.

Complete server-refusal/swapped-response acquisition and session restore/fork/
clear lifecycle remain open, including previous settings and carried effort.
Full model identity acquisition, model-rejection lifecycle and concrete telemetry
sinks, prefix-heal-declined acquisition, remaining repair priority/producers,
declared-tools model-cache admission, whole request bytes, fixed-thinking
property order, nonstream configured-chain advancement and the remaining release
inventory also remain open. Mod stays with the other branch and the full goal
stays active. Earlier waves below retain their historical scope.

The completed fast-refusal wave evaluated the full native **IR = GGt || x4e**
composition through `llm-client`. The SDK owns the first quoted-model match for
`speed` rejection and the case-sensitive fast-not-enabled envelope check. The
host supplies normalized request/target identities and process rejection history.
An accepted local refusal-cascade target now reaches main, fallback, reopened
stream and agent requests through scoped execution context without entering the
provider request body. Recognized target/history speed errors cannot start or
spend an unclaimed display trial. An effective fast request may instead retry
without fast mode before ordinary retry spending and decline-header handling.

The host separates **blocked** models from **ever-rejected** models. Enabling fast
mode again clears blocked models while retaining their rejection history; later
matching refusals can still take the native repair path. Blocked models have the
fast field and its coupled beta removed before dispatch. Wire error parsing stays
in `llm-client`; this wave adds no obsolete compatibility API.

The [fast-refusal oracle](../../scripts/tests/fast_refusal_288_oracle.mjs) executes
**2,520** actual IR/oFe/Yle cases and **12** native rejection-state cases across
**37** extracted source ranges. The identity corpus executes the actual bare-wire
qI/Bb/kt branch with served aliases, transport parsing and model overrides supplied
absent. Target, history and prefix-heal facts are explicit oracle inputs. The
[physical SDK regression](../../crates/llm-runtime/tests/fast_refusal_288_test.rs)
performs **25** dispatches across both request modes, including target/history
combinations, a mismatched quoted model, host target propagation, blocked-model
filtering and rearm with retained history.

Historical fast-refusal validation passed **8,355** tests and **31** checks, including
strict workspace/SDK Clippy and the prior **11** native CLI probes, with **9**
existing tests ignored. **67** unchanged source hashes and final command results
are recorded in `fast_refusal_current_validation`. The CLI probes revalidate
prior display, named-beta and pF behavior using fake credentials and loopback APIs;
they do not establish native fast-refusal target acquisition or live-provider
acceptance.

Full native $Fo acquisition from turn override, chain step versus user model and
server-latched fallback metadata remains open. So do full model identity
acquisition, host reset/clear/cold-resume lifecycle, concrete warning/telemetry
sinks and the prefix-heal-declined host flag. Remaining repair priority/producers,
classifier/safeguards, declared-tools model-cache admission, whole request bytes,
fixed-thinking property order, failed cost/notice durability and release inventory
also remain open. Mod stays with the other branch and the full goal stays active.
Earlier waves below retain their historical scope.

The completed pF wave made the first-party main Messages display probe evaluate the complete
native **21-term pF guard** through `llm-client`. Media, advisor, thinking type,
binding, tool-definition/change, cache-field, effort and output-configuration
refusals cannot start an unclaimed display trial. pF remains separate from GGt:
a pF-only error on the trial request still spends the failed-probe budget and
does not commit a healed conversation rejection. Hosted model-refusal
classification stays an explicit routing fact; native first-party Gft is unknown.

The [policy oracle](../../scripts/tests/error_policy_288_oracle.mjs) runs **4,140**
actual pF cases and **690** actual P4e/GGt state cases. It checks standard JSON,
top-level messages, text bodies, UTF-16 quantifiers, ASCII word/case rules and
ECMAScript whitespace without equating non-ASCII whitespace to literal spaces.
The [physical SDK regression](../../crates/llm-runtime/tests/error_policy_288_test.rs)
performs **50** dispatches across both request modes, including JSON/text errors,
near misses, failed-trial accounting and later conversation isolation. Error
payload projection now belongs to the SDK: text errors survive instead of
becoming null, while successful SSE/binary metadata keeps its own semantics.

[Four native CLI probes](../../scripts/tests/error_policy_live_288.py) confirm
media JSON/text, tool-tag and cache-field refusals: exactly one refused request
and one successful later-query request, both retaining display updates. The
prior seven display/named-beta probes also pass again. These are isolated
loopback tests with fake credentials. Hft acquisition and IR's x4e branch remain
explicitly outside this helper/probe corpus.

Historical pF validation passed **8,352** tests and **30** checks,
including **11** native CLI probes, with **9** existing tests ignored and
**57** frozen source hashes in `error_policy_current_validation`. Full repair
priority/producers, IR fast-refusal history, classifier/safeguards, model-cache
admission, complete request bytes and host lifecycle remain open. Mod remains
deferred, and the full goal stays active. Earlier waves retain their historical scope.

The completed named-beta wave follows native `kMe`, `SMe`, `Gt` and the earlier
first-party token-count repair through `llm-client`. A named rejection retires its
computed beta immediately, even when the retry fails. Strong refusals also mark
the host-supplied model identity for subsequent conversations; weak refusals stay
conversation-local. Computed betas are captured before explicit body overrides,
so an explicit extra-body beta cannot create an automatic retry loop. Resetting
context replaces the beta-state identity while retaining signature history;
an old in-flight request cannot reject the new conversation state. First-party
Messages gateways receive the ordinary computed betas, with custom providers
retaining their own policy.

The [beta oracle](../../scripts/tests/beta_repair_288_oracle.mjs) executes **6,336**
native named repair cases over standard JSON, top-level messages and text bodies,
plus **724** actual APIError/`GGt` recognition cases. The SDK matches the native
whole-envelope message bytes and first-line status-prefix removal. The
[physical SDK regression](../../crates/llm-runtime/tests/beta_repair_288_test.rs)
checks both request modes, failed retries, conversation versus model scope,
token-count priority and explicit-body ownership. [Three native CLI probes](../../scripts/tests/beta_repair_live_288.py)
confirm strong, weak and display-shape rejection sequences across user queries;
four display probes also pass again. They use fake credentials and loopback mocks.

The display evidence is corrected: native `Czt` is
`redact-thinking-2026-02-12`. The previous oracle supplied the connector-summary
beta under that name. It now extracts the actual declaration; the SDK removes
redaction when adding display updates and retains connector summarization.
The previous mutation claim with that false binding is superseded. The updated
display oracle passes all **2,206** cases against the native declaration.

Historical named-beta validation passed **8,347** tests and **25** checks,
including **7** native CLI probes, with **9** existing tests ignored.
**48** final source hashes and the affected revalidation are recorded
in `beta_repair_current_validation`. Complete IR/pF and repair priority,
declared-tools model-cache admission, server classifier/safeguards, whole request
bytes and host lifecycle remain open. The full goal remains active; Mod is deferred.
The earlier display wave below retains its historical scope with the correction above.

The completed display wave makes default first-party main Messages calls add `thinking.display:"updates"`
and `thinking-display-updates-2026-08-18` through `llm-client`, before the
explicit extra-body spread and request sealing. A gateway's unclaimed 400/422
may try once without the automatic field, even at zero ordinary retries and
with a retry-decline header. Only a healed trial commits the conversation latch.
An unrecognized failed trial increments the separate process budget; two failures
stop further probes without disabling the feature itself. Stream controller
opening commits before the first SSE frame; malformed nonstream success does not.
Side-query success cannot commit an inherited main probe. The host retains
conversation ownership; signatures and independent conversations keep separate
state. The branded flag is `LINGXI_THINKING_DISPLAY_UPDATES`.

The [display oracle](../../scripts/tests/thinking_display_288_oracle.mjs) runs
**2,206** native helper cases, including **192** exact thinking JSON/beta byte mutation
comparisons with supplied request shapes and **32** selected known context/signature/fast gates. These selected
facts preserve their native priority distinction: signature rejection belongs to
the policy guard, so it blocks the first probe but still counts a failed second
request. The [SDK regression](../../crates/llm-runtime/tests/thinking_display_288_test.rs)
covers both physical request modes, owned/reset conversations, known rejections,
failed trials, explicit extra thinking, provider/side-query isolation and typed
proxy simulation. [Four native CLI probes](../../scripts/tests/thinking_display_live_288.py)
confirm a healed request followed by another user query, plus simulation `0`,
`false` and `1`, against isolated loopback APIs with fake credentials.

Historical display validation passed **8,341** tests and **22** checks,
with **9** existing manual/doc tests ignored and **41** unchanged source
hashes in `thinking_display_current_validation`. Full IR/pF and beta repair
priority, classifier admission, model/flag/trust acquisition, complete envelopes
and host lifecycle remain open. The full goal stays active; Mod remains deferred.
Earlier waves below retain their historical validation scope.

The completed HTTP model fallback wave uses the query-owned chain before
ordinary retry spending. Non-529 5xx errors switch immediately unless the live
watchdog suppresses that gate. HTTP 529, typed overload payloads and stream-body overloads share a count;
the third switches even under the watchdog. Earlier retry exhaustion retains
the original typed error. Overload payloads and persistent capacity acceptance
precede retry-decline headers in both HTTP request modes. Initial stream opening
and a midstream reopen consume the settled query directive; hosted/stateful
execution retains the SDK replay guard.

`llm-client` now preserves network failure causes after output begins, including
its built-in HTTP body reader. Partial collection, terminal resource release and
provider decoder errors retain their own semantics. Raw HTTP 529 retains its
overload category even with another payload type. The [HTTP oracle](../../scripts/tests/http_fallback_288_oracle.mjs) executes
**5,600** native model gates and **32** native decline-priority cases with the
explicitly scoped dependencies in its fixture. The [SDK query regression](../../crates/orchestrator/tests/model_fallback_288_test.rs) covers
**33** scenarios; an [additional SDK regression](../../crates/llm-runtime/tests/http_decline_priority_288_test.rs) covers
**16** stream/nonstream decline combinations. [Thirteen native CLI probes](../../scripts/tests/http_fallback_live_288.py)
confirm model/input/notice/JSONL sequences, including body overload followed by
HTTP overload and a connection loss followed by HTTP 500.

Final selected validation passed **8,333** tests and **30** checks,
with **9** existing manual/doc tests ignored and **33** source hashes
frozen in `http_model_fallback_current_validation`. Full access/availability/
last-resort and nonstream query-chain behavior, model-window admission, failed
cost/notice lifecycle, concrete SDK host sinks and other release requirements
remain open. This is scoped evidence; the full alignment goal remains active.
Earlier waves below retain their historical scope and hashes.

Current configured thinking-only model fallback now follows the native query
route and chain decisions. A completed thinking block followed by a server or
provider timeout error advances an explicit fallback chain unless the persistent
watchdog suppresses that hop. The next model receives a fresh ordinary retry
ledger; tool continuations keep its route and the next user query starts with the
selected model again. The selected session settings remain unchanged. Model-aware
preparation sees the new route while consume-once context and the original input
messages remain intact. Provider-qualified and bare cross-provider routing remain
supported. A missing server retry hint no longer becomes an artificial one-second
floor that can terminate the only ordinary retry.

`llm-client` now exposes the terminal native `message_delta` as it arrives, so a
subsequent error observes the stop reason before `message_stop`. The host projects
it once. Typed model-fallback notices use the current flattened JSONL head, retain
record identity on resume, and expose the native SDK envelope through a typed
output callback and Core frame builder. Provider partial SSE is a separate output
path; concrete CLI sink adoption remains follow-up work. Known usage on the
abandoned attempt is submitted before the model hop; the token regression does
not establish full priced/durable native cost acceptance.

The [current oracle](../../scripts/tests/model_fallback_288_oracle.mjs) checks
**1,650** actual native chain and **1,176** notice/compact-format cases with the
explicitly supplied facts described in the fixture. The [real SDK regression](../../crates/orchestrator/tests/model_fallback_288_test.rs)
checks **12** scenarios, including budget reset, tool continuation, next-query
reset, provider routing, original-message equality, stop/persistent/output
negative controls and persisted/resumed notice identity. The [actual native CLI
probe](../../scripts/tests/model_fallback_live_288.py) checks **5** isolated
server/timeout/budget sequences with fake credentials and a loopback-only proxy,
including exact notice/JSONL order and retained request messages.

Final selected validation passed **8,327** tests and **54** checks,
with **9** manual/doc tests ignored and **60** source hashes frozen in
`configured_thinking_fallback_current_validation`. Other configured fallback
triggers, model-window/compaction admission, full failed-attempt lifecycle and
SDK host integration remain open. Mods stay with the other branch. Earlier waves
below retain their historical scope and hashes.

The current bounded nonstream recovery now consumes the native elapsed-time fact
from a failed stream. With no configured fallback model, a long no-output server
error reopens streaming twice before a permitted nonstream fallback. Persistent
work then stops after **3** eligible nonstream timeouts. An explicit branded limit
uses the native signed decimal schema and is re-read on every failure. Only a
transport timeout lasting at least **90%** of the selected deadline consumes it;
ordinary shared retries can exhaust earlier. Admission and settlement waits do
not inflate the measured provider work. The retry watchdog now reads only its
canonical branded setting; its old alias/helper was removed.

The [guard oracle](../../scripts/tests/nonstream_timeout_retries_288_oracle.mjs)
checks **16,000** actual native schema/duration/counter cases. Real SDK regressions
check **19** timeout policies, **2** independent-call resets and **12** full
stream-to-nonstream scenarios. The
[native CLI probe](../../scripts/tests/stream_timeout_recovery_live_288.py) sends
**3** long started/server-error streams followed by exactly **3** nonstream header
timeouts against a loopback mock with fake credentials, then exits with an error.
This validates the selected physical recovery sequence without a live provider.

The final direct Anthropic fallback `stream:false` parameter is now written by
`llm-client` before sealing. It overrides extra-body stream values, preserves the
property's order and removes its obsolete exact-string sidecars. All **12**
physical scenarios also pass with an explicit extra-body `stream:true` override.
**28** actual
AWo spread/JSON byte cases and an exact-string sidecar regression verify this
field. Hosted and other provider envelopes retain their own policy.

Current selected validation passed **6,712** tests and **49** checks,
with **9** existing manual/doc tests ignored. **22** unchanged source
hashes, SDK checks and the physical native report are recorded in
`bounded_nonstream_recovery_current_validation`. Configured model fallback,
remaining stream classifiers/branches, withheld notices and paid failed-attempt
durability remain open, alongside the other release inventory. Earlier waves
below retain their historical scope; this wave supersedes their affected hashes.

The current non-streaming duration now matches native `uzt`: **300s** locally,
**120s** in a remote session, with the current integer parser and **2,147,483,647ms**
upper bound. The branded remote setting uses the native typed boolean rules.
Scientific/grouped/prefix syntax and ECMAScript whitespace match; invalid negative
timeouts retain the SDK validation error and never dispatch. One logical nonstream
drive captures the duration once across its HTTP retries. The stream first-byte
timeout retains its independent 600s default.

The [duration oracle](../../scripts/tests/nonstream_timeout_288_oracle.mjs) checks
**5,280** actual native selection/parser/SDK-validation cases. The
[SDK regression](../../crates/llm-runtime/tests/nonstream_timeout_288_test.rs) checks
**22** header/body deadline scenarios, **4** pre-dispatch rejections and a retry
after an environment change. The
[native CLI probe](../../scripts/tests/nonstream_timeout_live_288.py) independently
checks **4** invalid settings with fake credentials, zero model requests and an
isolated loopback proxy. These probes do not establish live-provider timing.

The completed duration wave passed **4,571** tests and **44** checks,
with **1** existing manual test ignored and **11** unchanged source
hashes in `nonstream_duration_current_validation`. Long failed-stream elapsed-time
propagation, persistent watchdog three-timeout exhaustion and the complete native
fallback request envelope remain open. Prior waves below retain their original
scope; the current wave supersedes their affected production hashes.

The current shared retry ledger now owns stream stalls, truncations and completed-
thinking retries beside its HTTP retry count. **12,800** native decision cases
match both the pure helper and shared counter mutations. Production completed-
thinking recovery without a configured fallback uses this ledger: one provider
timeout retry, two server/connection retries and one watchdog retry, subject to
the same remaining HTTP budget. Connection delays use the specific 100ms counter;
watchdog retries reopen immediately. Abandoned thinking stays out of history; an
exhausted kept thinking reply emits a no-output notice and cannot trigger text-only
continuation. Visible output on a later attempt clears the prior retry decision.

The [physical SDK regression](../../crates/orchestrator/tests/thinking_stream_288_test.rs)
verifies **6** real adapter/service/SSE transport scenarios, including settings
limits and an earlier HTTP 500 consuming the shared budget. Every request stays
streaming. **4** mock cause scenarios verify terminal counts and retained history.
The completed thinking-ledger wave passed **2,416** tests and **42** checks;
**9** unchanged source hashes are in `thinking_ledger_current_validation`.
Configured model fallback, malformed/partial-output branches, long failed-stream
watchdog/nonstreaming limits, withheld notices and failed-attempt cost durability
remain open. SDK/runtime checks below retain their earlier foundation scope.

The timeout foundation now preserves SDK `timeout_error` as `ProviderTimeout`,
separately from a provider internal error, local transport timeout and watchdog
interruption. HTTP statuses stay explicit; SSE errors do not invent a 500 status
or prefix. The current kind inventory, failover, redaction, host conversion,
retry/telemetry and error envelopes retain this distinction. A complete terminal
delta can finish normally across the timeout error classes. Completed local tools
prevent `truncatedAfterOutput` and the text-only recovery nudge, and their result
sequence is drained once before the api-error terminal.

The [current stream oracle](../../scripts/tests/stream_recovery_288_oracle.mjs)
executes **12,800** actual `uWo/dFe/oH/nH/Xle` cases and **128** actual partial
notice/truncation producer cases. The pure Rust decisions match every decision
case; production shared-ledger/stream-driver integration remains open. Runtime
checks the applicable current notice and truncation forms; no-output/malformed
notices, withheld recoverable rows and the complete timeout/watchdog flow remain
open. The foundation does not establish the release's end-to-end timeout parity.

The completed timeout foundation passed **5,154** tests and **44** checks,
with **1** existing manual runtime test ignored. Evidence and
**19** unchanged source hashes are in `timeout_foundation_current_validation`.
The prior source-wave counts below retain their original validation scope; the
new wave supersedes affected production hashes.

The independent Fast-status source wave replaces the API-key-only credential
snapshot with `CredentialProvider::anthropic_auth_snapshot`. The current API
returns OAuth token/scopes and their refresh scope beside the cached/static key.
Status reads do not load model credentials, execute helpers or renew on expiry.
Desktop and mobile retain the restored OAuth delegate when a model API key wins;
desktop also retains profile-only/environment OAuth. Direct environment-key model
routes retain the independent status source. Model authorization keeps its selected
credential and never borrows a status key.

The [current source oracle](../../scripts/tests/fast_status_sources_288_oracle.mjs)
executes **576** actual `Qp/td/s2e/g` cases. Both SDK drivers verify **1,152** source
scenarios, independent environment-key routes, OAuth-owned observations across
model-key rotation, account observer invalidation, and real expiry/rejection/refresh/
logout behavior. The readonly expired token is queried first; an admitted rejection
refreshes using the OAuth scope before re-querying. Desktop verifies **14** actual
factory/driver scenarios, including profile-only status OAuth with a model API key.

The completed independent-source wave passed **5,150** tests and **43** checks,
with **1** existing manual runtime test ignored. Evidence and
**29** unchanged source hashes are in `independent_status_current_validation`.
Native full dispatch and cache/source lifecycle, physical mobile acceptance and
other latest changes remain open. The structured-output counts below describe the
previous completed wave; its hosted captures were not rerun for this source change.

The current structured-output wave implements independent experimental-beta and
branded structured-output disable gates. Native provider names `anthropicAws` and
`anthropicGoogleCloud` map to the supported Bedrock/Vertex protocol families;
automatic schemas are no longer dropped unconditionally on those routes. The
selected model capability and chronological model gate apply before SDK encoding.
The caller's schema stays intact for another provider. Explicit extra-body formats
remain governed by the current main/side merge rules.

The [structured-output oracle](../../scripts/tests/structured_output_288_oracle.mjs)
executes **31,518** actual typed getter, provider/model, main and side cases.
The runtime verifies **36,000** preparation projections, **64** physical stream/
nonstream captures, a same-service flag transition and other-provider controls.
Main explicit formats suppress automatic insertion; side extra spreads still own
the final config. Other LLM protocols retain their schemas and unsupported-capability
errors. HIPAA taint, resolved canonical identity and capability refusal are supplied
facts in the native helper; their full production acquisition remains open.

[Actual native hosted captures](../../scripts/tests/structured_output_live_288.py)
use `--bare`, isolated fake credentials, skipped cloud auth and a loopback scripted
400 response. Bedrock ordinary betas are in the header; its narrow extra beta subset
is in `anthropic_beta`. Vertex betas are in the header. Virtual `betas` is removed
from the body. Both retain the common `anthropic-version: 2023-06-01` header and
add their own body version. The SDK hosted codecs now retain that header, and the
host sends Bedrock model/request betas and applies the SDK virtual-parameter
normalizer on all four Claude families. These captures do not establish signed
or live cloud acceptance, nor full request-envelope equivalence.

The completed structured-output wave passed **5,148** tests and **45** checks, with **1** existing manual runtime test ignored. Evidence and **15** unchanged source hashes are recorded in `structured_output_current_validation`. Mantle/gateway route
identity and production capability/HIPAA acquisition, automatic title/recall/hook
caller selection, remaining Fast status source/cache lifecycle, timeout/compaction/resume
and other latest changes remain open. Mods remain deferred.

The official native sample passes manifest size and SHA-256 checks, and **2,184**
source chunks pass extraction/recheck. The [current OAuth oracle](../../scripts/tests/oauth_source_288_oracle.mjs)
runs **1,744** actual `ck/Lk/GK` cases, whose complete outputs equal the previous
native sample. The SDK selector now reads the 2.1.288 fixture and passes its current
regression, strict Clippy and format check. At that source audit, eleven prior-wave hashes matched; the selector test switched
to the new native fixture. The newer structured-output wave supersedes the affected
production source hashes.

The release also changes timeout continuation, compaction/resume durability,
source/model admission, instructions/hooks, MCP and LSP behavior. These areas remain
open pending actual source/transport evidence. All model communication stays in
`llm-client`. The current progress and distribution checksum are recorded in
[validation-2.1.288.json](validation-2.1.288.json).
