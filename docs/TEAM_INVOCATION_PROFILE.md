# Team invocation profile prerequisite

This is a host-owned, macOS-only runtime building block. **T3 execution stays
disabled.** It is not a Team executor, authenticated broker or complete process
isolation implementation. Product clients still use the previous Harness pin.

## Host contract

Only the trusted host constructs `desktop::team_invocation::TeamInvocationProfile`.
It has no settings, renderer, wire or serialization constructor. `prepare` mints
fresh tool/quota authority and must be called once for a consumed authenticated
start permit, never to replay an ambiguous invocation after restart.

Supply a canonical, host-controlled 0700 storage root. The profile derives a
SHA-256 namespace from length-delimited owner UID, Bot key, Team ID, conversation
ID, **original human UID** and that human's membership generation. A generation
alone is not an identity. It creates private home/workspace/tmp/shell-home/inputs,
transcripts and per-invocation output directories. Symlinks, non-private modes,
wrong ownership, sandbox glob characters and corrupted input snapshots fail
closed. Host-selected persona/Skill text is bounded, hashed and stored as a
regular 0400 snapshot; live owner Skill directories are never copied.

Set an explicit `provider/model`, with only that selected provider's credentials
and endpoint configuration. The profile discards other provider profiles and
unrelated Anthropic credentials. Team credential bootstrap uses a fresh memory
store, never native/file credentials. A host supplying a shared credential stack
must itself restrict its memory store to the selected credential; a memory
backend label does not prove its contents. Generic provider environment fallback,
ambient OAuth/PAT capture, API-key/proxy/AWS refresh helpers and owner identity
metadata are disabled on the Team assembly and public headless service paths.
The SDK still has process-level environment readers: a clean, dedicated host
process and request-level verification remain prerequisites for full credential
isolation. No real provider request is validated by the fixture tests.

Call `validate_input` on the **fully composed human text** before submission, then
`apply_to` on the host config. Assembly and public headless service entry points
reapply the profile before loading credentials/settings. They reconstruct the
config from defaults; they do not inherit owner callbacks, resume state, folders,
MCP, plugins, hooks, startup tasks, skills, prompt shell expansion, native tools
or settings watchers. The profile owns its policy cache across clones/reapply;
an owner's initialized cache is not reused. Managed policy can deny capabilities
but cannot grant shell execution on the owner's behalf.

Host Git probes are also disabled at subagent-renderer construction, prompt/
environment rendering, transcript branch stamping and server-classifier context.
A model-owned `.git` pointer or `core.fsmonitor` must never cause the host to
inspect another repository or execute a command outside the shell sandbox.

Only Bash and, when explicitly configured, AskUserQuestion can be registered.
The revocable tool lease also filters dynamic tools. Every Bash command requests
host approval, including read-only commands; managed allow rules and sandbox
auto-approval cannot substitute for that approval. The product must bind the
permission adapter exclusively to the authenticated owner. Do not expose its
resolver through generic Team-member or remote-control IPC.

No question sender is inherited from `DesktopConfig`. Without an explicit
`with_original_human_question_sender`, questionnaire registration fails closed.
That method accepts a trusted host channel, not an authenticated broker: the
product must implement a separate broker checking original human UID/generation,
question identity, membership, revocation and the shared deadline for every answer.
An owner/TUI/remote-control answer channel is not a substitute.

Automatic-memory retrieval and session-memory extraction use explicit paths under
the same private config home, sharing the main model service/quota. Ambient
HOME/LINGXI_CONFIG_DIR and ancestor instruction discovery are not their roots.
Transcripts and shell output spools also stay under the private home. Real model
memory retrieval/extraction remains untested.

## Shell boundary

The immutable shell policy requires a working macOS sandbox. The Team wrapper
generates its own profile instead of using the general desktop sandbox manager's
owner-home/global-temp write grants. Only the private workspace/tmp are mutable;
private inputs/home/shell-home remain read-only. Fixed system executables and
libraries are readable. Network and shared system credential/UI IPC have no
grants. Other platforms reject profile creation.

Both outer and wrapped Bash use `/bin/bash --noprofile --norc -c` with a cleared
environment, a fixed PATH and private HOME/TMPDIR. Owner startup files, loader
variables and model credentials are not inherited by these shell children.
The model cannot disable the sandbox, expand grants with `.git/commondir`, persist
`cd`, use host Git snapshots or trigger unconfined cwd readback. Explicit/automatic
background handoff is disabled. Each command is capped at 120 seconds.

Private foreground completion and cancellation kill their owned process group,
including same-group children whose parent already exited. **This does not prove
termination of descendants escaping through setsid or other detach mechanisms.**
There is no completed five-minute process supervisor or CPU/memory/disk limit.

## Bounds and revocation

| Limit | Default | Host ceiling |
| --- | --- | --- |
| Agent turns | 12 | 20 |
| Composed human input UTF-8 bytes | 32,768 | 32,768 |
| Snapshot instruction UTF-8 bytes | 65,536 | 65,536 |
| Output tokens per physical request | 4,096 | 4,096 |
| Physical generation dispatches | 24 | 32 |

Clones/reapplication share the same tool lease and `ScopeOutputQuota`; they never
reset spent authority. `revoke` blocks captured tools and future model dispatches,
not already admitted OS/model work. See [the output-quota contract](SCOPE_OUTPUT_QUOTA.md)
for provider-cap assumptions and entry-point restrictions. The input byte check
does not bound history/tool output, total model input/tokens, money or runtime.

## Required before enabling T3

1. A one-shot-start consumer and fresh invocation process with a clean environment,
   selected credentials, narrow host transport and actual request inspection.
2. Owner-only permissions and a separate authenticated original-human question
   broker, including membership/revocation checks after every asynchronous wait.
3. A total five-minute deadline covering queue start, model/tools and approvals,
   with reliable process termination including detached descendants.
4. Durable result publication/retry and owner recovery for ambiguous work. No
   automatic tool replay, new quota or start authority after an uncertain crash.
5. Desktop/Web/iOS/Android binding, trigger, question and recovery UI; real model,
   crash/offline/rejoin/revocation and complete native/product acceptance.

Fixture verification commands are recorded with exact results and source hashes
in the product's `acceptance/teams-t3-profile-20261029/` checkpoint. Passing these
checks does not satisfy the remaining executor or release gates.
