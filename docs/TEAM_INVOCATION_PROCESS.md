# Team invocation process prerequisite

`harness-team-worker` and `desktop::team_process` provide a fresh macOS process
for one trusted host invocation. **T3 remains disabled.** This is not a backend
listener, public IPC endpoint, authenticated approval broker or durable executor.
Product dependency pins and launch paths do not consume this local companion.

## Trusted host boundary

The host must authenticate and consume `startBot`'s one-shot `execute:true`
response, preserve its trigger/attempt fencing and stable invocation UUID, and
capture `leaseUntil` with `TeamProcessDeadline::from_backend_unix_millis` when
the response arrives. Requests have no serde/settings constructor. This API does
not authenticate the caller or a backend response. A member, renderer, ordinary
Bot router or remote-control client must not construct these inputs.

Construct `TeamProcessRequest` from the verified human principal, immutable
persona/Skill snapshot, fully composed human text, finite limits, selected model
and that deadline. `TeamProcessStart::prepare` applies the private profile's
storage and input checks. `run` consumes the launch and uses a canonical,
host-owned executable that is not writable by group/others. The host must
select its trusted built worker; file ownership alone does not authenticate an
arbitrary executable or protect it against replacement by the same OS user.

Before spawning, the parent creates and syncs an owner-only `process-started`
tombstone under that invocation's private output directory, including syncing
the directory. Concurrent starts and restarts with that same UUID fail closed.
Spawn failure, pipe failure, cancellation, timeout and a lost result never remove
the tombstone or authorize execution again. It contains no credential or prompt.
Stable backend identity mapping and cross-host fencing remain product duties;
choosing another UUID/root is not an authorized recovery mechanism.

## Process and transport

Every invocation executes the dedicated binary, with no CLI options or shared
worker pool. The parent clears the environment, fixes private home/config/tmp,
cwd, PATH and locale, and supplies only the selected model credential through
an anonymous stdin pipe. The binary checks its environment before loading the
bootstrap or building the runtime. The eight exact environment keys also fix
CoreFoundation's private user home and UTF-8 encoding. Pre-main framework
initialization must not recover the owner's encoding preferences or add a
user-derived value after `env_clear`. It closes all inherited descriptors above
stdio at its fresh single-threaded entry, including descriptors without CLOEXEC,
using actual `/dev/fd` entries rather than a current-rlimit assumption. Do not
call the binary's entry function inside an existing host process.

The only inbound frame is a versioned, length-prefixed bootstrap, limited to
256 KiB. Unknown fields, invalid versions/limits, truncated or oversized frames
refuse. The pipe remains open as a parent-liveness lease; EOF or any extra byte
cancels work. There is no second-start, permission, question, settings, registry,
credential-refresh, resume or command-dispatch message. The private format is
never registered on product IPC. Stderr is discarded, and fixed nonzero exit
categories contain no provider bodies or diagnostics.
The exit categories are 65 (protocol), 74 (I/O), 75 (deadline/cancellation),
77 (profile), 78 (environment) and 70 (other worker failure).

Only a selected static Anthropic API key and qualified Anthropic model are
currently supported. Other protocols/OAuth/cloud/subscription auth require their
own explicit credential transfer and real wire validation before admission.
The host chooses an HTTPS endpoint; literal loopback HTTP is allowed for local
gateways and acceptance fixtures. URL userinfo, queries and fragments refuse.
No key is placed in argv, environment, a settings file or a credential sidecar.
Provider configuration, model routing, memory and transcripts use the preceding
[private profile contract](TEAM_INVOCATION_PROFILE.md).

The child constructs a new private runtime/quota exactly once and submits one
human turn through the real SessionService. **All tool requests are denied** by
the worker's permission pump. No owner approval resolver is exported, and no
original-human question sender is installed. This restriction must remain until
the authenticated owner and original-human brokers are implemented and tested.
Consequently this prerequisite never executes model-selected shell commands.

## Deadlines and results

The parent accepts an absolute lease only within the next five minutes and
converts it once to a monotonic deadline. Waiting before launch never extends
it. Bootstrap writes, output reads, child waiting, build/model/drain work and
future provider requests share the deadline; cancellation or dropping the
parent future kills its worker process group, with kill-on-drop as a fallback.
The child also checks the absolute lease and supervises its runtime and parent
pipe independently. Async stdin is deliberately avoided because its blocking
reader can prevent runtime shutdown.

At expiry, the parent sends SIGKILL and separately permits up to two seconds for
reaping; it never accepts a late result. This is not a proof of hard real-time
termination during scheduler/OS/filesystem stalls. Detached tool descendants,
CPU/memory/disk ceilings and general cross-platform process containment remain
gates before allowing tools. Earlier private Bash group cleanup does not solve
`setsid`/detach escape. The current worker denies every tool rather than relying
on incomplete tree containment.

Only one bounded terminal result may leave stdout. Text is limited to 8192 UTF-8
bytes, matching the backend result ceiling; cumulative received text also has a
conservative 8192-byte cap across retries, not a token or billing bound. The
reply comes from the last accepted assistant message, preserving the core's
visible-text rules. Retracted attempts and intermediate tool preambles cannot
become the Bot reply. Typed completed-turn API-error facts, abnormal/missing
natural stops and an empty final answer cannot be reported as Completed.
Provider errors and overflow return a fixed outcome with empty text. The host
must map failed/limited outcomes to an appropriate fixed settlement message;
it must not publish raw error bodies.

Terminal delivery/ACK loss is still ambiguous: no automatic retry or quota
reset is performed. Persisting results, idempotent `finishBot` retries,
revocation polling, recovery UI and four-client integration remain product work.

## Validation boundary

The integration suite runs the actual built worker and pinned SDK against only
synthetic loopback HTTP/SSE fixtures. It checks selected credentials/request
caps/private prompts, ambient environment exclusion, denied model tool calls,
quota exhaustion, retries/typed terminal errors, finite result text, inherited
FD positive controls, deadlines/cancellation/drop, parent-pipe loss, malformed
bootstrap and duplicate/concurrent launch refusal. No real model credential,
Firebase account, public deployment, signed product or device flow is exercised.
