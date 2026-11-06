# Shared Bot M1 runtime

Private transport major 3 is the only Team host/worker contract. The old owner/bot-key/requester identity, invocation-only transcript, claim/start/finish connector, shell/question broker, and combined 32 KiB input are removed.

`BotChatScope` is Team × SharedBot × Chat × sessionEpoch. Its deterministic UUID owns the existing private canonical `home/projects/.../<uuid>.jsonl` transcript and `home/session-state/<uuid>` journal. Each task still runs in a fresh process with a fresh output quota and five-minute deadline; completed rounds resume the same session with `headless::session::prepare` and captured-history mounting. This does not resume or retry an interrupted provider turn.

Desktop owns the shared Firestore source cache and backend lifecycle. It reads the latest 200 messages for the initial batch, then the complete `(lastMessageSeq,currentMessageSeq]` range and source changes through the same watermark. Runtime accepts the strict `ChatInputBatch`, including required `changes`, original author identity/display name, creation/edit time, message/reply/source IDs, and both sequence domains. Immutable private references bind the complete bytes with SHA-256 rather than imposing a 256 KiB control-frame limit on chat input. A private transport object has an explicit 64 MiB resource ceiling; exceeding it fails without cursor movement or truncation. Canonical session and branch-recovery journal readers use the existing configurable 128 MiB record bound, so the journal's ordinary 4 MiB default is not an additional hidden input limit. Model context capacity remains a separate preflight gate.

The existing session journal commits one ChatInput fact and both cursors. Existing durable transcript append-once then projects each source message separately. Projection repair is allowed before another model turn; replaying a committed batch never grants another execution permit. Source edits/recalls use the existing compact-boundary chain mechanism to rebuild a model projection from current original messages, dropping old summaries and derived assistant context. Original immutable journal/transcript facts are retained. Accepted output task receipts deduplicate the Bot's subsequent shared Chat echo.

Preflight uses the runtime context estimate and model capacity with output/prompt reserve. If necessary it invokes the existing compactor on previously consumed history before committing the current batch. The current admitted input is protected from automatic/reactive compaction. Capacity refusal leaves the input cursor unchanged. Every actual provider dispatch, including auxiliary compaction and retries, awaits the existing scope quota's narrow host-authorization callback before spending a physical dispatch.

M1 tools are `TeamChatHistory` and `TeamKnowledge`. The first obtains permission-checked pages from the shared Chat broker; the second reads only the published immutable knowledge snapshot. Neither exposes owner memory, filesystem tools, scripts, owner MCP registry, delegation, approval/wait/continuation, or writes. Provider account selection and credential refresh still use existing provider-account APIs; the trusted parent validates live owner state and materializes only the selected pool under the private home before it reaches the worker.

The final accepted Markdown is a deterministic document result. Runtime persists the complete terminal result through the existing transcript append-once API before returning it over the pipe. `recover_result` reads that exact terminal receipt after a lost pipe acknowledgement without performing a model call. Desktop/backend still own publication, artifact ACL, exact result retry, and delivery idempotency. A process-start tombstone refuses replay of ambiguous computation.

## Validation status

The default Desktop host/worker build and library check passed. Focused final results currently recorded:

| Check | Result | Evidence |
| --- | --- | --- |
| Actual isolated worker fixtures | 15 passed in 303.22s | `/tmp/teams-runtime-process-final.log` |
| Scope/profile isolation | 7 passed | `/tmp/teams-runtime-profile-final.log` |
| Data reference and history contract | 4 passed | `/tmp/teams-runtime-contract-final.log` |
| Actual major-3 host entry | 3 passed | `/tmp/teams-runtime-connector-final.log` |
| Canonical Chat input journal | 7 passed | `/tmp/teams-runtime-journal-final.log` |
| Private account/model/result contracts | 12 passed | `/tmp/teams-runtime-private-units-final.log` |
| Per-dispatch scope authorization/quota | 4 passed | `/tmp/teams-runtime-quota-final.log` |
| Existing durable transcript writer | 27 passed | `/tmp/teams-session-writer-final.log` |
| Existing compaction crate | 301 passed | `/tmp/teams-compaction-final.log` |
| Actual Desktop JS → host → worker pipeline | passed | `/private/tmp/teams-host-runtime-final-2979258a.log`; receipt `/private/var/folders/2c/gch8wj4s0d52rygkyd6b9f_h0000gn/T/shared-bot-v3-integration-f9r9WX/receipt.json` |
| Existing orchestrator compaction subset | 7 passed, 1 failed | `/tmp/teams-orchestrator-compaction-regressions.log`; baseline oracle discrepancy documented in the cleanup receipt |

The final actual-worker suite passed all 15 tests. It covers completed-round persistence, full 200-message bootstrap and 450-message delta, original author/time metadata, edit/recall rebuild and another restart, journal-before-projection repair, exact own-echo suppression, real read-only knowledge/history calls, denied shell calls, credential/environment isolation, cancellation, physical authorization refusal, preflight capacity refusal, provider failure/oversize refusal, observed partial usage, and exact terminal recovery. The preflight compaction case also denies a summary dispatch with no provider call or cursor movement, then verifies successful summary/main dispatch accounting and a further compacted restart without reintroducing summarized source rows. Fixture bytes are copied into the runtime repository, not loaded through a product-checkout dependency: `shared-bot-execution-vectors.json` SHA-256 `328bbd0f4f5e80789c94cd2121ef58b34297f2c0ba3204ceea63569090734ebb`, matching the product contract file.

Fixtures use synthetic loopback provider responses and ephemeral credentials. They do not establish real-model, signed Desktop package, Android/iOS device, production token, or long-running acceptance. Per-task usage uses the existing cost ledger delta; it is reported as partial when observed and missing otherwise because the aggregate snapshot does not retain every provider response's usage completeness. No missing usage is replaced with zero.

Projection still appends each newly admitted source row through the existing durable append-once writer. An ephemeral verified index avoids replaying all prior rows on each round, but new-row durability has a per-row cost in the debug build. The Desktop integration measured 25,876 ms for 200 messages and 141,771 ms for the full 450-message delta, using genuine 300-second task deadlines. It observed two model calls, six authorizations, cursor revisions 1 and 2, and exact result recovery with zero additional model calls. Before/after binary SHA-256 matched: host `d260a1888ece9e6cadf0ffebd54bae7ebf73a23a149af0f6558b4f0db16c819b`, worker `2979258ac744520ea991a81646513e3acd21b8a6acf86094888c8c6b2de8d481`. This implementation does not claim a production latency benchmark.
