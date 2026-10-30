# Trusted Team connector prerequisite

`desktop::team_connector` drives one host-selected backend queue identity on
macOS. It connects the authenticated Teams HTTP protocol to the dedicated
`harness-team-worker`. **T3 remains disabled.** There is no product startup
registration, renderer/Remote Control endpoint, background discovery service,
tool-approval broker or original-human question broker. All worker tools remain
denied. Product dependency pins do not consume this unpublished companion yet.

## Authority and authentication

The host creates one `TeamConnector` for its current owner UID and account epoch,
a fixed trusted `/v1/teams/messages` endpoint, a session cancellation token,
canonical owner-owned 0700 journal/runtime roots and its trusted built worker.
The two roots must be distinct and stable across restarts. Never choose another
root, backend alias, trigger or UUID to bypass an ambiguous invocation. HTTPS is
required except literal loopback HTTP for local acceptance. Userinfo, queries,
fragments, redirects, ambient proxies and transport retries are excluded.

`TeamConnectorAccount` is a trusted host port, not a public IPC implementation.
The host must cancel the session token synchronously on logout/account change,
including logout/relogin with the same UID. `is_current` checks UID/epoch before
and after awaited credential refresh and HTTP reads. Each credential must match
both. Token acquisition, request and body reads share a five-second bound; each
request and response JSON body is limited to 64 KiB. HTTP errors expose only the status.
No Firebase ID token is persisted, placed in argv or supplied to the model
worker. Only the selected static Anthropic credential crosses its private pipe.

The backend still verifies the Firebase ID token and derives current membership,
Bot ownership, binding revision, original-human membership and source evidence
inside its transaction. Local target metadata and claim response text grant no
execution authority. Only a fresh `startBot` response with `execute:true` can
launch a worker. That one-shot response now contains a versioned execution
snapshot from the same transaction. Repeated starts of the same still-running
attempt return no snapshot and `execute:false`; terminal/recovery receipts omit
the execution field. The connector checks every target/attempt/binding field, the
original message-to-trigger SHA-256 identity, finite sequence/generations and
the original human UID/text before preparing a private runtime. Human UID rules
match the backend's UTF-16 length and slash/C0/DEL exclusion, including Unicode
and punctuation; Bot owner/key identifiers retain their narrower format.

## Durable state and crash boundaries

The journal key hashes backend endpoint, owner, team, conversation, Bot and
trigger. Binding revision and owner generation are deliberately excluded from
the key: changing them cannot create new execution authority for the same
trigger. The immutable intent records and validates both. One random attempt ID
is synced before claim; a stable worker invocation UUID derives from the key.
The backend owns cross-host fencing. A nonblocking advisory file lock excludes
concurrent local drivers and releases automatically on crash. It is not the
source of execution authority.

Every record is create-new, owner-owned 0400, regular and singly linked. Reads
use `O_NOFOLLOW`, bound bytes and reject partial/unknown JSON. Directories are
canonical 0700. Files and directories are synced before the next phase:

1. `intent.json` precedes claim. A failed claim may be retried with that same
   attempt; it has not authorized execution.
2. `start-requested.json` precedes **any** start dispatch. If its response is
   lost, invalid or false, a restart only observes the old attempt. It never
   claims/starts/constructs another worker or quota. A partial marker also fails
   closed. A local start marker does not prove the server received a request.
3. `permit.json` stores the authenticated execution snapshot and original
   absolute deadline, without credentials. It is read back before worker launch.
   A saved permit never authorizes execution after restart. The process layer's
   independent `process-started` tombstone still precedes spawning.
4. `result.json` stores the exact terminal settlement before `finishBot` is sent.
   `retry_result` accepts no model/snapshot and can only repeat that same result.
   `run` also detects saved results and takes this result-only path. Backend
   deterministic receipts prevent a second reply after an ACK is lost.

Ordinary completed worker text may settle as `completed`. Other typed worker
outcomes settle a fixed `failed` message, withholding provider diagnostics.
Ambiguous worker errors, cancellation and deadlines have no automatic result
or replay. A new host may retry only a durable exact result, never model/tools.
Partial journal writes, mismatched bindings and unavailable storage require
owner intervention. No automatic deletion or journal migration is implemented.

The owner can now explicitly recover an **expired claimed** run as well as an
expired running/recovery run. This covers a start intent whose credential refresh
failed before HTTP dispatch. Active claims/runs refuse recovery; the transaction
fences a racing reclaim/start. The operation settles a reply and never runs tools.
A 409 while observing such an abandoned attempt reports `RecoveryRequired`.
The connector does not invoke owner recovery automatically.

## Lifetime and remaining gates

The five-minute backend lease is captured once into the process layer's
monotonic deadline before journal/profile work. A separate HTTP monitor polls
immediately, then waits two seconds after each successful response, requires the
original lease, and never renews it. Revocation, stale authorization,
malformed/changed leases or failed polls cancel/kill the worker group and perform
bounded cleanup. Account and explicit cancellation use that same path. Cleanup
allows up to two seconds for reaping; completion is not guaranteed on timeout.
Dropping the driver invokes kill guards without awaiting reaping. No automatic
finish publishes an ambiguous result. Backend `finishBot` rechecks
authorization atomically before publishing even if revocation races completion.
Observation of a change can take the two-second delay plus the five-second
token/request bound, followed by cleanup. Timers,
filesystem and OS scheduling are not hard real-time termination guarantees.

This is a single-queue-item executor port, not the full always-on host. Product
account integration, durable queue discovery, UI recovery, authenticated owner
approvals, original-human questions, other provider/auth routes, detached process
containment before allowing tools, independent resource/billing limits, all four
T3 client UIs, signed native packages and physical/live acceptance remain gates.
The private journal is trusted host state; this is not a defense against a
malicious process running as the same OS user or host-selected wrong executable.

## Validation

The connector suite exercises the actual worker and pinned SDK with synthetic
loopback HTTP/SSE, including selected credentials, adversarial claim bodies,
strict permits, durable restart/retry, live cancellation/revocation, lock and
storage corruption cases. The explicitly ignored `actual_firebase_backend_and_worker`
case must be run separately by the product's isolated emulator fixture; a normal
test run does not establish Firebase authentication/transaction integration.
That fixture uses only `demo-lingxi-teams-t0`, real Auth-emulator ID tokens, the
actual authenticated backend handler/Firestore transactions and the actual worker
against a synthetic model. No production account/provider/deployment is involved.
