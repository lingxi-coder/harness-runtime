# Headless native baseline

The pinned baseline is **Claude Code 2.1.293, darwin-arm64**, SHA-256
`4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`.
The binary is 236330608 bytes and prints `2.1.293 (Claude Code)`.

At 2026-10-08T00:03:21Z, the official [latest release API](https://api.github.com/repos/anthropics/claude-code/releases/latest)
returned `v2.1.293`, `prerelease: false`, `draft: false`, published
2026-10-07T18:10:20Z. The official [native latest pointer](https://downloads.claude.ai/claude-code-releases/latest)
also returned `2.1.293`. The downloaded binary matched the darwin-arm64 checksum
in the [versioned native manifest](https://downloads.claude.ai/claude-code-releases/2.1.293/manifest.json).
The manifest commit is `3abc54a9d60b4d12c627afad22d6e5f58a6199d2`.
The official `https://claude.ai/install.sh` supplied this download base URL; the
installer was read, not executed. The version, platform and checksum are pinned
once in `headless-native-baseline.json`; the runner does not chase later releases.
The old 2.1.289 Mods fixtures remain historical evidence, not this baseline.

## Run and compare

```sh
node scripts/tests/headless_native_differential.mjs
node --test scripts/tests/headless-fixtures/capture.test.mjs
node scripts/tests/headless_native_differential.mjs \
  --execute /tmp/harness-claude-code-2.1.293-darwin-arm64 \
  --output /tmp/headless-native-new-evidence
node scripts/tests/headless_native_differential.mjs \
  --execute /tmp/harness-claude-code-2.1.293-darwin-arm64 \
  --harness-config /absolute/path/to/isolated-harness-host.json \
  --rules /absolute/path/to/reviewed-byte-rules.json \
  --output /tmp/headless-differential-new-evidence
```

Without `--execute`, the runner lists the pinned baseline, matrix, configuration
and rules format without launching Native or opening a server. `--only id,id`
selects fixtures; `--timeout-ms` is bounded to 1000–60000ms per process. An output
directory must be new. All native-native repetitions and comparisons finish
before the first Harness invocation. Each phase has its own comparison context;
identity correlations span every artifact in a fixture, including resume.

The Harness configuration contains an absolute `command`, optional `args` and
`flags` arrays, and explicit `env`. For example:

```json
{"command":"/absolute/path/to/isolated-host","args":[],"flags":[],"env":{"LINGXI_CONFIG_DIR":"{{state}}","LINGXI_API_BASE_URL":"{{provider}}"}}
```

The host must actually route model traffic through the supplied loopback URL
and use the supplied isolated state directory. This is an adapter contract;
the example is not proof that a shipping host implements that configuration.
`{{state}}`, `{{workspace}}` and `{{provider}}` are the only substitutions.
Both hosts receive `--bare`, which disables keychain reads and automatic user
customization, and the same ordinary print/SDK arguments. Explicit regular-startup
fixtures remove `--bare` on both sides and use an owned empty `HOME`, local hooks,
local MCP and an explicit local plugin. The process environment is constructed
from scratch, with no inherited `HOME`, credentials, proxies or provider settings.
New captures always use `state/isolated-home` on both engines, including bare
fixtures; historical captures retain their original environment in invocation
receipts. This prevents host home-directory fallback without masking any static
wire difference.
The only credential is a fixed dummy
API key, and additional provider base URLs must equal the local provider URL.
The server binds only `127.0.0.1`, verifies the dummy key, accepts explicit health
and Messages routes, rejects unexpected traffic, and never forwards requests.
Shell fixtures can only print a fixed string or write `permission-marker.txt`
inside the fresh fixture workspace.

## Matrix and captures

The 28 declarative fixtures cover text, JSON, verbose JSON, stream JSON, partial
chunks, Unicode, text stdin, persistence disabled, SDK initialize, user replay,
two sequential turns, a second-process resume, structured output, turn and budget
limits, automatic permission denial, host approval/denial, interrupt and malformed
input, basic fork, selected resource/UI controls, regular hooks/MCP startup,
explicit local plugin catalog and JSON publication versus SessionEnd timing.
This extends the controlled SSE and sequential result-driven input used by
`mods_native_acceptance_289_loopback.mjs` and
`mods_native_utf16_citations_289_loopback.mjs`; no existing historical fixture is
rewritten or silently treated as a latest-version test.

For each process the evidence retains `stdin.bin`, `stdout.bin`, `stderr.bin`,
`exit.json`, the invocation, observation, event timings and session JSONL. A
result triggers the next user turn or input EOF immediately. Process close,
remaining fixture-process cleanup and session snapshots happen afterward. No
synthetic wire event stands in for cleanup or completion. Session discovery
walks only the fresh owned state directory, ignores symlinks and captures only
the explicit fixture session UUID and its child session directory. A fork's new
session UUID is accepted only from its actual `system/init` output. Its original
filename and a bijection to the other run's observed init UUID are reported;
unknown additional session files remain significant.

Each local HTTP transaction retains its exact request wire, header bytes, body
bytes and response bytes. The response fixture offered by the server is recorded
separately: when an interrupted client disconnects before the delayed response
write, actual `response.bin` is empty. Response bytes mean bytes submitted to
the socket; they do not establish that an aborted client consumed them. Provider
metadata records the process index, transaction order and whether a response
write was attempted. The owned workspace/config/tmp are removed after capture;
raw evidence, the pinned binary, unrelated `/tmp` files and user caches remain.

## Observed acceptance boundary

The current 28 cases were captured in several bounded batches, with **60 selected
native subprocesses** across two repetitions per case, including resume/fork
subprocesses. The original 20-case/42-process batch is
`/tmp/headless-baseline-293-final/report.json`; later selected batches add schema
retry, SDK controls, ordinary startup, plugin, verbose JSON, fork and shutdown
timing cases. Every selected capture completed with the expected exit/result,
no timeouts and no unexpected provider requests. This is a stitched evidence
cohort, not an assertion that one later full batch was executed. The durable
receipt is `scripts/tests/headless-fixtures/native-2.1.293-observations.json`.
Recorder and wrapper tests passed **18/18**; together with the strict comparator, the current Node suite passes **54/54**.

These are protocol/capture checks, **not Native-Harness byte parity**. No Harness
host was supplied to these runs. With zero normalization rules, the original 20
complete artifact comparisons report native nondeterminism. Text stdout itself is exact;
the differences include UUIDs/timestamps in session JSONL, UUIDs and durations
in JSON/stream output, initialize PID, permission request IDs, and a freshly
generated device ID embedded inside provider `metadata.user_id`. Each artifact
has lengths, SHA-256 values and the first differing byte. Unknown or malformed
normalization rules fail closed.

The first real Native-Harness capture is separately recorded in
`headless-fixtures/native-harness-first-print-text.json`, using the confirmed
private run-002 binary and its recorded runtime/SDK SHAs and product diagnostic
overlays. It is a bounded v2 CLI proof, not validation of current product UI
changes. Fresh native-native print-text passed the frozen normalized comparison
with the same owned HOME paths. Harness offered 22 tools despite `--tools ""`,
including an unrequested StructuredOutput tool, then injected a schema-enforcement
retry prompt without `--json-schema`. It produced no stdout/stderr, made six
provider calls, and required timeout cleanup. Raw header case differed first at
offset 38; the request body differed at offset 90 after the registered device-ID
normalization. Session comparison rejected missing leaf-UUID rule hits and
retained an additional internal ledger artifact as a failure. No rules were
expanded and no golden evidence was refreshed. Broader Harness cases await
triage of this first completion failure.

The frozen `headless-fixtures/normalization.json` explicitly selects UUID,
timestamp, timing, hook duration and PID fields by exact paths and hit counts.
The independently tested wrapper replaces only the device-ID scalar inside
`metadata.user_id` while preserving outer escapes, nested structure, account,
session and all surrounding bytes. For a fork, its session scalar and the one
`X-Claude-Code-Session-Id` header are normalized only after the bijection was
established from the actual init frames. Socket paths must exactly match
`/tmp/cc-socks/<observed-process-pid>.sock` and belong to an init frame. HTTP
request lines, other headers, spelling, order and framing remain significant.
These are comparison buffers; the recorded raw files are never overwritten.

The remaining first differences are recorded in the durable receipt and rule
plan. They include an optional `ui_invalidate` record and session JSONL
ordering/reference differences. A two-case 200ms provider-delay probe did not
remove the identity violation: the first `last-prompt.leafUuid` referred to
`session_context` in one run and the date attachment in the other. The comparator
preserves that reference relationship rather than replacing each UUID independently.
No record is reordered or omitted to obtain a passing result.

Notable native observations:

* Nonverbose JSON is a single result object; verbose JSON is an ordered array
  containing init, assistant and result messages. A tool-denial case includes
  the ordered user tool-result and second assistant message. Hook start/response
  messages remain absent from verbose JSON even with `--include-hook-events`;
  the ordinary stream fixture emits those real hook frames.
* Stream JSON includes a native builtin `system/ui_invalidate` event before init
  even in `--bare` mode. It is retained as native evidence, not invented by the
  recorder.
* SDK permission callbacks require `--permission-prompt-tool stdio` as well as
  `--permission-prompts host`. Both host approval fixtures observed an actual
  `can_use_tool` request. Merely reaching a final result does not satisfy them.
* Turn limit returns exit 1, `error_max_turns`, `terminal_reason: max_turns`;
  budget limit returns exit 1, `error_max_budget_usd`, `budget_exhausted`.
* An interrupted streaming request returns exit 1, `error_during_execution`,
  `aborted_streaming`; its delayed local response was never written.
* Two invalid StructuredOutput calls with `MAX_STRUCTURED_OUTPUT_RETRIES=2`
  return `error_max_structured_output_retries`,
  `structured_output_retry_exhausted`, exit 1 and `num_turns: 3`. The second
  provider request continues the same query with the failed tool-use/tool-result
  pair; no new retry user prompt was inserted.
* Verbose JSON arrived as one complete 4274-byte chunk before SessionEnd hook
  begin by 66ms and 57ms in two captures; the owned hook then waited 200ms before
  process close. This proves publication before the observed SessionEnd hook,
  without assuming every native internal cleanup substage was exercised.
* The explicit plugin catalog is ordered local plugin first, then the two builtin
  plugins. Local descriptors contain `name,path,source,version` with source
  `headless-local-plugin@inline`; builtin descriptors omit `version`.
* The basic fork retained inherited message UUIDs and parent links, rewrote their
  `sessionId` to the new fork UUID and had no observed `forkedFrom` field. This
  fixture had no file-history snapshots/backups, worktree or agent-setting data.

Rules are explicit per phase, fixture and artifact:

```json
{"version":1,"nativeNative":{"print-json":{"processes/0/stdout.bin":[
  {"id":"result-uuid","path":"/uuid","type":"string","mode":"identity","group":"message","count":1,"validate":"uuid"}
]}},"nativeHarness":{}}
```

This example only replaces the UUID leaf and therefore still reports timing
differences. No default rule erases all UUIDs, timestamps, arbitrary JSON strings,
object key order, scalar spelling, null versus omission, or raw provider headers.
Replacing the whole nested-JSON `metadata.user_id` string as a single identity
would conceal device/account/session structure and is never applied.
Reviewed, precisely scoped dynamic rules and a real Harness capture are required
before `parityEstablished` can be true. A subset run can establish only its
per-fixture result; the top-level full-matrix verdict remains false. Existing
captures can be compared without launching a process or rewriting evidence:

```sh
node scripts/tests/headless_native_differential.mjs \
  --compare-existing /tmp/headless-baseline-293-final \
  --rules scripts/tests/headless-fixtures/normalization.json
```

Unverified groups remain explicit: queued `send_now` delivering/stopped and
`cancel_queued` branches; dynamic SDK MCP manifests/tool-list changes; actual UI
render/event/key responses; model/permission/thinking control mutations; CLI
invalid format combinations and continue/missing or ambiguous resume; signal
and closed-pipe behavior; hook error/deny and latest-version Mods modules;
file-history/backups/worktree/agent-setting fork inheritance and persistence
disabled during fork; broader JSON Schema keywords/dialects and retry-budget edge
values beyond the eight probes described below. The captured idle/unknown-ID control responses do not establish those
other branches.

Eight separately selected schema research probes now have a durable receipt at
`headless-fixtures/native-2.1.293-schema-probes.json`; they are excluded from the
default 28-case acceptance matrix. Invalid StructuredOutput with retry limits
`0` and `-1` performs one provider call, then reports the literal configured
attempt count. `2tail` uses two attempts; `nope` falls back to five. Missing
StructuredOutput appends exactly one `[structured-output-enforce]` user-text
reminder, then a second ordinary end-turn response completes successfully with
`structured_output` omitted. A valid StructuredOutput wins over `--max-turns 1`
and `2`, with one provider request and native `num_turns: 2`. Text mode prints
the compact JSON projection plus one newline, exactly 38 bytes in this fixture.
Broader schema keyword/dialect coverage remains unverified.

Additional scheduling candidates preserve the original frozen rules: waiting
200ms after SDK initialize acknowledgement yielded a passing multi-turn pair,
but initialize still differed in session row order (`queue-operation` versus
attachment). Waiting 500ms before text stdin still differed in its first session
row (`atis-latch` versus `last-prompt`). These observations do not establish a
repeatable full matrix. `--startup-settle-ms`, `--text-stdin` and
`--provider-delay-ms` record controlled timing/input candidates; they do not
filter output or session records.

Bounded embedded-source receipts are saved in
`headless-fixtures/native-2.1.293-source-metrics.json`, with binary byte offsets
and SHA256 for each excerpt. Native result timings use one admitted-query
`performance.now()` anchor: the first top-level assistant receipt, first
`message_start`, first content block start or delta, and first request dispatch.
The observations persist across later tool-loop requests and are rounded and
clamped to zero; these optional timing fields are populated only for a successful
non-deferred result. Partial-stream `ttft_ms` has a separate API-attempt clock.

The same source identifies session-scoped running subagent totals for Agent
calls, excluding forked skills, workflows, teammates and other internal agents.
Each spawn has a guarded terminal outcome; resume starts fresh and `/clear`
resets the totals. Safety-stop totals count recognized refusal/content-filter
events across the query pipeline, including subagent/internal calls; they are
not persisted and can increase more than once for one API call. These source
receipts establish the measurement and counter contracts; they do not establish
Native-Harness equality.

Supplemental controlled lifecycle probes are saved in
`headless-fixtures/native-2.1.293-lifecycle-probes.json`. Two stream JSON runs
emitted the first parent result before the delayed background child completed,
with `spawned: 1` and `completed: 0`; stdin EOF followed that first result.
Native then admitted the child completion notification as an internal query,
emitted a second result with `completed: 1`, and exited. Single native captures
of nonverbose JSON and text emitted only the final notification result.
Verbose JSON emitted an eight-record array retaining both result frames after
the assistant frames, with both results refreshed to `completed: 1`; task
lifecycle system events were absent from that array. Two ordinary refusal
probes, followed by deterministic retry success, each reported `safety_stops: 1`.
These research cases remain separate from the 28-case acceptance matrix.

`--lifecycle-probes` explicitly selects this research matrix; `--native-once`
is allowed only with that matrix and forbids a Harness invocation. Its reports
mark native-native comparison as not run and never establish stability or parity.
Parent/child responses match actual request message content, with an eight-call
ceiling, rather than relying on request arrival order. Native `--bare` removes
Agent through its simple-mode tool catalog, so the Agent cases use normal
startup with an empty owned HOME.

Text diagnostics and two repeated paused-stdout SIGINT probes per output mode
are recorded in `headless-fixtures/native-2.1.293-text-sigint-probes.json`.
Native text provider errors include a newline; max-turns and structured-retry
errors use explicit diagnostic strings without a newline. The source has a
separate execution-error branch and budget-error formatter. Execution-error
stderr comes only from the auto-mode-unavailable stop notice latch, not a generic
error ring or an arbitrary result error. With stdout kept
open and paused while a 2 MiB response filled its buffers, native text and
stream JSON (including requested partial events) each exited naturally with
code 0 about two seconds after SIGINT in both runs. Raw output remained
truncated. The pinned print signal handler explicitly requests exit 0 and marks
stdout as externally clocked; its drain races against a two-second deadline.
This establishes bounded native behavior in these controlled cases, not whole
output stability, TTY/hook behavior or Harness parity.

A single inline SDK MCP manifest research capture is recorded in
`headless-fixtures/native-2.1.293-mcp-ui-metadata.json`. Its connected server
status retained the tool's `ui` metadata object (including custom keys) and
legacy `ui/resourceUri`, omitted unrelated private metadata, and included empty
annotations. This does not verify invalid URI/visibility cases, dynamic catalog
invalidation or Native-Harness metadata equality.

The remaining send-now and request-marker capability contracts have bounded
source receipts in `headless-fixtures/native-2.1.293-source-capabilities.json`.
Send-now distinguishes waiting, delivering and stopped requests and bypasses
Stop's pending-abort latch and background sweep. Marker lists preserve actual
client prompt consumption order, cap the list at 64, and stamp the first main
assistant and first non-ping partial event independently across a whole turn.
Actual command `started` consumption, rather than mere queue admission, adds a
folded prompt to that list.

Two fixed-client-UUID research captures in
`headless-fixtures/native-2.1.293-client-marker-probes.json` verify success and
max-turns marker placement. Tool continuation does not restamp later assistant
or partial frames with an unchanged primary UUID. Success result markers are
adjacent; the max-turns error result places the scalar UUID before `type` and the
list after `uuid`. A preparatory SDK HTTP400 marker capture was incomplete and
is explicitly excluded. Eligible queue batching, meta-fold and 64-entry edge cases and Harness equality
remain unverified. A separate completed HTTP400 JSON capture confirms native
`terminal_reason: "api_error"`, `stop_reason: "stop_sequence"`, success subtype
with `is_error: true`, and status 400; those fields do not derive from an HTTP
status threshold alone, since the source uses the final synthetic API-error
assistant as the terminal discriminator.

Two repeated SDK input research cases are recorded in
`headless-fixtures/native-2.1.293-input-queue-probes.json`. Two fixed external UUID
user frames in one stdin write produced two separate turns in both native runs.
A second human frame submitted on an assistant tool-use frame, while a bounded
local Bash tool ran, was folded into that query in both runs. The next provider
request included the exact native human-message wrapper in a system message
after the tool result. The final result kept the original primary UUID and added
the consumed UUID to its list; later assistant frames were not restamped.
The consumed prompt lifecycle started after its replay user frame and before
the next requesting status. Its completion preceded the final result, followed
by the primary command completion. These observations do not establish all
eligible queue batching boundaries. The source receipt separately shows an
ordinary completed-response return before the post-tool queued-attachment fold
path.

A separate two-repeat multi-human midtool probe is recorded in
`headless-fixtures/native-2.1.293-input-multifold-probes.json`. Native retained
one queued-command attachment per human message, each with its original prompt,
source UUID, delivery ID, reminder rendering and system role, followed by one
correlated removal record per consumed command. The provider request combined
the two rendered wrappers in order with two newlines between them, without the
reminder tags. These human rows did not use `renderedByBatchHead`; the source
restricts that optimization to verified Slack human relay messages. The earlier
single-message JSONL receipt remains preserved separately.

Token updates have source-only receipts in
`headless-fixtures/native-2.1.293-source-auth-updates.json`. The session access
token serves ingress; model OAuth resolution reads the OAuth token separately.
Runtime updates preserve an explicitly empty string. Subsequent truthiness-based
lookup falls back to the corresponding ingress or OAuth file/store source;
OAuth updates also invalidate its cached resolution. These are source contracts,
not fresh authentication runtime acceptance.

Five narrow parser research cases, each repeated twice, are recorded in
`headless-fixtures/native-2.1.293-parser-probes.json`. Bare `--tools default`
selected Bash, Edit and Read in that order. Plain stdin and positional prompt
were joined with one newline while retaining stdin's trailing newline. Invalid
partial/JSON, replay/text and stream-input/JSON combinations exited with code 1,
empty stdout and exact stderr diagnostics. These are native behavioral receipts,
not successful full-artifact stability or Harness acceptance.

The first real comparison's symmetry audit confirms identical arguments,
empty stdin, workspace, isolated HOME and first offered/written provider
response. Its Harness prompt newline, reminders, extra tool catalog and schema
continuation therefore are not explained by runner input asymmetry. The
private binary remains identified by its recorded checksum; later runtime fixes
require a rebuilt binary and a fresh capture.

Existing authorized branding, multiple providers, Fusion typed scalar prompts
and Create Local App differences remain as documented in
`claude-alignment-closeout-plan-2026-10-07.md`. This lane neither expands those
exceptions nor edits production Rust, Cargo files, the byte comparator, or LLM
ownership boundaries.
