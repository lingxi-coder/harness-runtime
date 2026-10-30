# Private Team invocation process

`desktop::team_process` launches one clean macOS `harness-team-worker` per trusted
start. `TeamProcessStart::run` preserves the v1 all-tools-denied route.
`run_with_broker` selects v2 for explicit owner permissions and original-human
questions. The API is not an authentication boundary or public IPC decoder; the
[trusted connector](TEAM_CONNECTOR.md) supplies those authorities. Packaging,
publication, and live acceptance remain separate from the source described here.

## One-shot launch and clean process

The host must authenticate/consume `startBot execute:true`, preserve its
trigger/attempt fencing and stable invocation UUID, and capture `leaseUntil`
once with `TeamProcessDeadline::from_backend_unix_millis`. `TeamProcessRequest`
has no settings/wire constructor. Its identity, immutable persona/Skill snapshot,
fully composed human text, finite limits, model, and deadline come from the
trusted host. A member, renderer, ordinary Bot router, or Remote Control caller
cannot mint a request.

`prepare` validates the private profile and input. Before spawning, the parent
creates/syncs a private `process-started` tombstone and its directory. Concurrent
or restarted launches with that UUID refuse, including after spawn/pipe failure,
cancellation, timeout, or a lost result. The tombstone contains no prompt/key and
is never automatically deleted. Changing UUID/root is not authorized recovery.
The executable must be canonical, owner-owned, and not group/other writable;
the host must also select the correct trusted build. This does not defend
against a malicious process sharing that OS account.

The worker has no model-role CLI options or shared pool. The parent clears its
environment and fixes private HOME/config/TMPDIR/cwd, PATH, locale, and
CoreFoundation home/encoding. At fresh single-threaded entry, the worker checks
all eight keys and closes inherited descriptors above stdio using `/dev/fd`,
including descriptors without CLOEXEC. Never call its entry point inside an
existing host process. The selected model key travels only in an anonymous
bootstrap pipe; stderr is discarded. HTTPS is required except literal loopback
HTTP for gateways/fixtures. URL userinfo/query/fragment refuse.

`TeamModelRoute` freezes the native SDK registry's resolved model, protocol,
endpoint, sanitized singleton provider profile and exact external credential ID.
The existing codecs admit Anthropic Messages, OpenAI Chat/Responses, Gemini
GenerateContent/Interactions, Foundry Claude and Azure OpenAI with static API-key
or bearer auth; Azure additionally requires its configured API version. OAuth,
managed credentials, delegate/fallback routes and embedded static secrets refuse.
Arbitrary extra headers/body and sibling connections are removed. Selected model
capabilities, inference settings and deployment facts remain intact. Bootstrap
requires the complete route; missing fields never select an Anthropic default.
The worker rewrites only the credential reference to its single private in-memory
slot. Its key never appears in the frozen profile or persistent snapshot.
Codec admission and synthetic fixtures do not establish live provider acceptance.

## Private framed protocol and brokers

Frames use a four-byte big-endian length and a bounded JSON body, at most
256 KiB. Unknown fields, invalid versions/limits, zero/oversized/truncated frames
refuse. Both versions accept exactly one bootstrap and retain stdin as a parent
liveness lease. EOF cancels live model/tool work. A dedicated blocking OS thread
reads stdin so shutdown cannot hang on Tokio's blocking stdin task.

Version 1 permits only a terminal result on stdout. Every tool is denied, no
question sender is installed, and any further inbound byte cancels execution.
Version 2 permits typed owner-permission/human-question requests and one terminal
result on stdout. Parent replies can resolve only a pending interaction of the
same type and exact ID. There is no second-start, settings, registry, resume,
credential-refresh, or arbitrary command route. The format is not registered on
product IPC. IDs are unique across both interaction channels for the invocation,
1–128 printable ASCII bytes. Interactions are serialized; unknown, duplicate,
wrong-kind, out-of-order, or oversized traffic fails closed. The parent also
rejects any worker byte/EOF while awaiting a broker callback.

The trusted host implements `TeamProcessBroker::approve` and `ask`. Callbacks
run inline under the captured deadline/cancellation, receive revocable child
tokens, and are dropped with the parent future. Only Bash can request owner
approval; the gate resolves AllowOnce or Deny against its original JSON input.
No Always/Auto decision, edited input, policy mutation, or generic owner/TUI
question route is available. Broker-enabled workers explicitly install
`with_original_human_question_sender`; default workers never inherit one.

Questions contain 1–4 unique original texts of at most 2000 UTF-16 units each.
Their exact texts are answer-map keys. Headers, options, and descriptions appear
only in rendered text bounded to 8192 UTF-8 bytes. Answers must use exactly those
keys and nonempty strings of at most 2000 UTF-16 units, with a bounded aggregate
encoding. Duplicate JSON keys and lossy lone-surrogate question projections
refuse; validated strings are projected back to the tool's UTF-16 carrier.
An explicit absent answer drops that dedicated responder. No broker events or
tool arguments are added to the process journal; ordinary private transcripts
retain their existing behavior.

## Lifetime, tools, and terminal results

The absolute lease must initially lie within the next five minutes and is
captured once into a monotonic deadline. Waiting never renews it. Bootstrap,
build/model/tool work, both broker waits, output reads, shutdown, and worker
waiting share it. Cancellation/drop/expiry kills the worker group with
kill-on-drop fallback; reaping gets a separate two-second bound. The child
independently supervises the lease/deadline and revokes its private tool/quota
authority during teardown. Late results never become successful completion.

For v2 Bash, a fresh private shell guardian receives the bounded approved,
sandbox-wrapped command without the model key. Only the worker holds the helper's
stdin lease. Worker death, parent loss, cancellation, command completion, and the
original deadline trigger shell-group cleanup. The guardian pins the group ID
with an unreaped leader, kills/verifies no live members before reaping, and
returns a successful tool result only after verified cleanup. The private macOS
policy denies `setsid`, `setpgid`, and `posix_spawn` while allowing ordinary
fork/exec. See the [profile contract](TEAM_INVOCATION_PROFILE.md). Scheduler/OS
stalls, CPU/memory/disk quotas, cgroups, and billing ceilings are not guaranteed.

Only one terminal result is accepted, after clean stdout EOF and successful
worker exit. Text is at most 8192 UTF-8 bytes; cumulative received model text has
a separate conservative 8192-byte bound across retries. The reply uses the last
accepted assistant, retaining visible-text/retraction rules. Missing/abnormal
natural stops, typed API-error facts, and empty final answers cannot report
Completed. Provider failures/overflow carry fixed typed outcomes with empty
text. Dispatch counts cannot exceed the original quota. Model-role exit codes
are 65 protocol, 74 I/O, 75 cancellation/deadline, 77 profile, 78 environment, and
70 other failure; no provider diagnostics leave the process.

A lost terminal result is ambiguous and never authorizes model/tool replay or
quota reset. Once the connector has durably saved an exact settlement, its
strict `retry_result` API retries only `finishBot`. Host observational retry uses
`observe_saved`: existing start markers are only polled, and a prelaunch journal
with no start/result returns Unstarted without model or HTTP work. Neither status
nor result retry authorizes execution; see the connector journal contract.

## Evidence boundary

Existing prerequisite checkpoints describe their own source snapshots and
all-tools-denied tests. Current suites additionally exercise owner AllowOnce/
Deny, original-human answers, malformed/replayed replies, callback lifetime,
admitted shell/child cleanup after worker SIGKILL/EOF/deadline/drop, and normal
return with residual children. Test definitions alone are not passing results.
The delivery owner records current commands/results/source hashes separately.
Synthetic loopback/native evidence does not establish real provider/Firebase
accounts, published pins, signed packages, device flows, or public deployment.
