# Private Team invocation profile

`desktop::team_invocation::TeamInvocationProfile` supplies fresh private storage,
configuration, tool authority, and output quota for a trusted macOS Team start.
The [connector](TEAM_CONNECTOR.md) consumes authenticated backend authority;
the [worker](TEAM_INVOCATION_PROCESS.md) enforces fresh-process lifetime and
interaction transport. This profile itself does not authenticate a caller or
prove packaged/live acceptance.

## Identity, storage, and configuration

Only the trusted host constructs the profile; it has no settings, renderer,
public-wire, or serialization constructor. `prepare` mints fresh tool/quota
authority once for a consumed start permit. Never reconstruct it to replay an
ambiguous invocation. Its canonical host-controlled storage root is 0700. The
namespace hashes length-delimited owner UID, Bot key, Team/conversation IDs,
original human UID, and that human's membership generation; a generation alone
is not an identity.

Private home/workspace/tmp/shell-home/inputs, transcripts, and per-invocation
output paths are created/validated. Symlinks, wrong owners/modes, sandbox glob
characters, and corrupted snapshots fail closed. Persona/Skill text is bounded,
hashed, and saved as a regular 0400 immutable snapshot; owner Skill directories
are not copied.

`validate_input` checks fully composed human text before submission. `apply_to`
and assembly/headless entry points reapply the profile before settings or
credential loading. Configuration is reconstructed from defaults rather than
inheriting owner callbacks, resume state, folders, MCP/plugins/hooks/startup
work, skills, prompt shell expansion, native tools, or settings watchers.
Clones share the profile-owned policy cache and authority; owner initialized
caches are not reused. Managed policy may deny but cannot grant shell execution.

Only the selected provider/model profile can supply credentials/endpoint.
Credential bootstrap uses a fresh memory store, never native/file credentials.
A host supplying a credential stack must restrict its contents, not merely its
backend label. Generic environment fallback, ambient OAuth/PAT capture,
API-key/proxy/AWS refresh helpers, and owner identity metadata are disabled.
The supported process route is selected static Anthropic only, with its key
supplied over a private pipe. SDK environment readers also require the clean
fresh worker; profile configuration inside an owner process is insufficient.
No real-provider behavior follows from synthetic fixture acceptance.

Host Git probes are disabled in renderers/prompts, transcript branch stamping,
and server classification. Model-owned `.git` pointers or fsmonitor settings
cannot authorize host probes or commands. Memory retrieval/extraction use
explicit private-home paths and share the main model/quota; ambient HOME,
LINGXI_CONFIG_DIR, and ancestor instructions are not their roots. Transcripts
and tool-output spools remain private. Real model memory behavior requires
separate acceptance.

## Tools and human authority

Only Bash and explicitly configured AskUserQuestion can be registered; the
revocable tool lease also filters dynamic tools. Every Bash invocation,
including read-only commands, requires explicit owner approval. Managed allow
rules or sandbox auto-approval cannot replace it. The broker-enabled worker
resolves only AllowOnce/Deny with unchanged original input, never Always/Auto or
a persistent rule. Its permission port is bound to the authenticated owner,
not ordinary Team-member/renderer/Remote Control input.

No question sender is inherited from `DesktopConfig`.
`with_original_human_question_sender` accepts only an explicitly supplied host
channel; without it question registration fails closed. Worker v2 supplies that
channel through the connector's separately authenticated original-human backend
broker. Exact question identity/text keys, original-human UID/generation,
membership, account epoch, revocation, and the original lease are rechecked
around waits. An owner/TUI answer is not a substitute. The default worker v1
continues to deny all tools and installs no question sender.

## macOS shell and crash boundary

The immutable shell policy requires the working private macOS sandbox and
rejects other platforms. It generates its own profile rather than inheriting
ordinary desktop owner-home/global-temp grants. Only workspace/tmp are mutable;
inputs/home/shell-home remain read-only. Fixed system executables/libraries are
readable; network, system credential/UI services, and shared IPC have no grants.

Outer/wrapped Bash use `/bin/bash --noprofile --norc -c`, cleared environments,
fixed PATH, and private HOME/TMPDIR. No owner startup files, loader variables,
or model keys are inherited. Model input cannot disable sandboxing, expand
filesystem grants through Git metadata, persist cwd, use host Git snapshots, or
invoke unconfined cwd readback. Explicit/automatic background handoff is disabled;
each command is capped at 120 seconds within the original invocation deadline.
The private SBPL denies Darwin `setsid`, `setpgid`, and `posix_spawn` syscall
entries, including the spawn-attribute escape route. Ordinary fork/exec remain
available; unsupported profiles fail closed rather than dropping restrictions.

Broker-enabled fresh workers enable a private per-command shell guardian before
runtime assembly. The helper is a distinct role of the trusted worker binary,
validates its clean environment/command/scope, closes inherited descriptors
before Tokio, and receives no model credential. Only the worker owns its stdin
lease writer. Abrupt worker death closes that lease independently of the dead
worker's cleanup code; cancellation, EOF, normal return, and deadline also clean
the command group.

The guardian keeps its direct shell leader unreaped as a PID/PGID anchor, kills
the group, verifies no live group members remain, then disarms/reaps the anchor.
Success is withheld on cleanup failure; it does not infer completion from pipe
EOF or reconstruct ancestry by polling. Residual same-group children are removed
even if the shell returns with their stdio redirected. The policy prevents model
processes from escaping that group via the admitted detach mechanisms. Cleanup
has finite bounds and is not hard real-time during OS/scheduler/filesystem
stalls. This is not a cross-platform resource supervisor, cgroup implementation,
CPU/memory/disk quota, or billing guarantee.

## Bounds and revocation

| Limit | Default | Host ceiling |
| --- | --- | --- |
| Agent turns | 12 | 20 |
| Composed human input UTF-8 bytes | 32,768 | 32,768 |
| Snapshot instruction UTF-8 bytes | 65,536 | 65,536 |
| Output tokens per physical request | 4,096 | 4,096 |
| Physical generation dispatches | 24 | 32 |

Clones/reapplication share the tool lease and `ScopeOutputQuota`; neither resets
spent authority. `revoke` blocks captured tools and future model dispatches; it
alone is not proof that previously admitted OS/model work stopped. The fresh
worker/guardian cancellation path handles that lifetime. See
[SCOPE_OUTPUT_QUOTA.md](SCOPE_OUTPUT_QUOTA.md) for provider-cap assumptions. Input
bounds do not bound history/tool output, total input tokens, money, or CPU use.
The worker additionally applies its original absolute five-minute lease,
bounded interaction frames/questions/answers, and 8192-byte final/cumulative
model-text limits described in the process contract.

## Evidence and remaining delivery gates

Historical `acceptance/teams-t3-profile-20261029/` checkpoints remain evidence
only for their recorded source/commands. The current worker, interaction,
connector/host, and native process-boundary suites add synthetic coverage for
the new brokers and guardian. Their current execution/review status must be
reported with source hashes; definitions or older passes do not prove a new pass.

Release delivery still requires verified product pins/publication, correct fresh
host launch/account lifecycle, signed native packages, four-client binding/
trigger/question/recovery behavior, and real crash/offline/rejoin/revocation,
model, device, and public-network acceptance. No production Firebase account,
provider request, deployment, or physical acceptance is established by this
profile document.
