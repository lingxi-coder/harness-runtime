# Trusted Team host and connector

`harness-team-host` runs a fresh macOS host for one host-selected backend queue
identity. `desktop::team_connector` authenticates and consumes the backend permit,
launches the private model worker, brokers its interactions, and journals terminal
delivery. This document describes current source contracts. Product dependency
publication, packaged launch behavior, and live acceptance require separate
verification; historical prerequisite acceptance is not evidence for these additions.

## Host authority and credentials

The trusted product main process launches the host with an empty inherited
environment, private home/config/tmp, fixed PATH/locale, anonymous pipes, and no
CLI options. The host validates the eight environment keys and their values and
closes inherited descriptors above stdio before creating runtime threads. It
accepts one version-one `start` line in `run` or `retry_result` mode. Private JSON
lines are bounded to 256 KiB and never registered as renderer, Team-member, or
Remote Control IPC. Stderr and fixed terminal errors do not expose credentials,
provider diagnostics, prompts, or response bodies.

The host emits correlated credential and owner-permission requests. Their shared
sequence increases monotonically in write order, including concurrent requests;
responses must match a pending ID, version, and kind. Unknown, duplicate, stale,
wrong-kind, malformed, oversized responses or parent EOF cancel the host. No
human-answer, second-start, settings, registry, resume, or command-dispatch route
exists on this channel. A dedicated blocking reader avoids Tokio stdin shutdown
hangs. Only bounded live interaction state is retained; broker payloads and tool
arguments are not added to the durable connector journal.

`TeamConnectorAccount` binds every Firebase credential to the captured owner UID
and account epoch. The product must cancel synchronously on logout/account
change, including logout/relogin with the same UID. `is_current` and credential
UID/epoch are checked before and after asynchronous acquisition/HTTP work. Token
acquisition, request, and bounded response reads share a five-second limit; HTTP
request/response JSON is limited to 64 KiB. The endpoint is fixed HTTPS, with
literal loopback HTTP allowed for fixtures. Userinfo, queries, fragments,
redirects, ambient proxies, and implicit HTTP retries are excluded. Firebase ID
tokens remain in the trusted host; only the selected static Anthropic key crosses
the separate model-worker pipe. Keys are absent from argv, environment, settings,
and journal records.

Only fresh `startBot execute:true` authorizes execution. The backend derives Bot
ownership, binding revision, original-human membership/generation, source
identity, and ACLs transactionally. Its one-shot execution snapshot is checked
against the exact target, attempt, binding, source-message/trigger SHA-256, and
finite identity/sequence fields. Repeated starts omit execution authority.
Human UID validation uses backend-compatible UTF-16 bounds and slash/C0/DEL
exclusions; Bot identifiers retain their narrower format.

## Separate owner and original-human interactions

`with_owner_approval` installs a trusted owner-only port and selects worker
protocol v2. Without it, the connector retains the v1 all-tools-denied route.
The adapter accepts only Bash and returns AllowOnce or Deny for the original
input. It offers no AllowAlways, Auto-mode transition, edited input, or persistent
rule. Backend running-state/lease and account checks surround the owner wait.
A Team member or generic owner/TUI/Remote Control answer path cannot supply this
capability.

Questions instead use authenticated `botQuestion` and `botPoll` backend
transactions, fenced by target, attempt, original human, membership generation,
question identity, and the original lease. The owner permission port never
answers them. Exact original question texts are answer-map keys; rendered
headers/options/descriptions are carried separately in bounded text. Every
asynchronous wait remains under the same worker cancellation/deadline, and the
adapter rechecks authorization before accepting a response. See the
[private worker protocol](TEAM_INVOCATION_PROCESS.md) for bounds and correlation.

## Journal and result-only retries

The journal key hashes endpoint, owner, team, conversation, Bot, and trigger.
Binding revision/generation changes cannot create another key for that trigger;
the immutable intent validates them. One random attempt is synced before claim,
and a stable worker UUID derives from the key. Backend fencing provides
cross-host authority. A nonblocking advisory lock excludes concurrent local
drivers. Journal/runtime roots must be distinct, canonical, stable, and private.

Records are create-new, owner-owned 0400 regular files with one link. Bounded
`O_NOFOLLOW` reads reject partial or unknown JSON; directories are canonical
0700. Files and directories are synced before advancing:

1. `intent.json` precedes claim. A failed claim may retry only its same attempt.
2. `start-requested.json` precedes any start dispatch. A lost, invalid, or false
   response never permits another start, worker, or quota on restart.
3. `permit.json` saves the verified snapshot and original deadline without keys.
   Reading it cannot authorize a restarted worker. The worker's independent
   `process-started` tombstone still precedes spawning.
4. `result.json` saves the exact terminal settlement before `finishBot`.
   `retry_result` needs no model/snapshot and sends no claim/start or model/tool
   work. `run_optional` detects a saved result before requiring a new model key.

The product persists its pending index before launching the host, so a crash can
leave an index with no Rust start marker. Host mode `retry_result` therefore calls
`observe_saved`: it locks/validates the journal, retries an exact saved result,
or only polls an existing `start-requested.json` attempt. With neither record it
returns `Unstarted` / `status:unstarted`, without HTTP, credential acquisition,
model construction, claim/start, worker, or new quota. The product accepts that
status only for an observational retry and may clear the stale prelaunch index;
it does not grant execution authority. The strict Rust `retry_result` API still
requires a saved terminal result. Existing/partial start markers never become
Unstarted or authorize replay.

Only Completed worker text settles as completed. Other typed outcomes use a
fixed failed message; ambiguous worker errors/cancellation/deadline return
RecoveryRequired without publishing invented text. A transport failure, HTTP 429,
or 5xx while delivering a saved result yields `ResultPending` / `RESULT_PENDING`.
Retry only that exact durable result; deterministic backend receipts prevent a
second reply after ACK loss. Invalid journals and mismatched bindings require
owner intervention. There is no automatic deletion, migration, or execution
replay. Explicit owner recovery settles expired claimed/running/recovery attempts
without tools; active leases refuse it, and the connector never invokes it
implicitly.

## Lease, containment, and validation

The backend's absolute lease is captured once as a monotonic deadline before
profile/journal work. Polling begins immediately and then waits two seconds after
each successful response; changed leases, failed polls, revocation, account loss,
or explicit cancellation stop the worker. Authorization checks also guard final
publication. Observation can take the polling delay plus the five-second HTTP
bound. Cleanup/reaping has separate finite bounds; timers, filesystems, and OS
scheduling provide no hard real-time guarantee.

Broker-enabled Bash uses the private macOS shell guardian described in the
[profile contract](TEAM_INVOCATION_PROFILE.md). Worker death closes its lease;
verified group cleanup precedes a successful tool result. This does not impose
CPU/memory/disk cgroups or a billing ceiling. Only macOS with the working private
sandbox and the selected static Anthropic route is admitted. Other platforms,
provider protocols, OAuth/cloud/subscription credentials, and real-model behavior
need their own implementation and acceptance.

The connector, host, worker-interaction, and native process-boundary suites use
synthetic loopback HTTP/SSE and owned fixture processes. They cover permit,
credential, journal/retry, interaction, cancellation, and containment paths. The
ignored `actual_firebase_backend_and_worker` fixture requires a separate isolated
Auth/Firestore emulator run using `demo-lingxi-teams-t0`; an ordinary test run does
not establish that integration. Current test execution, source hashes, product
pins, signed packages, four-client behavior, publication, and physical/public
network acceptance must be reported separately by the delivery owner. No
production account/provider/deployment is implied by these source contracts.
