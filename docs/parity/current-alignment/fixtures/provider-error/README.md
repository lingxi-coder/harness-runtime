# Native provider HTTP error text oracle

Run from any directory with the pinned executable passed explicitly:

    CLAUDE_NATIVE_BINARY=/path/to/claude python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/provider-error/run-native.py

cases.json fixes the request prompt, model, session IDs, HTTP status, response
body bytes and the lone-surrogate UTF-16 units. It covers a nested
error.message with a lone surrogate, an empty response body, and a null nested
error.message. Each case receives one HTTP 400 from a 127.0.0.1-only stub. The
runner verifies the Native SHA, uses a dummy
API key, isolated HOME/config and a one-dollar budget cap, and records POST
count to prove whether the CLI retried. It never saves the full model request
body.

The runner executes each case twice with the same prompt, model, fixed session
ID and exact loopback response body: once with `--no-session-persistence`, then
once with Native's default session persistence enabled. Both outputs and full
session files stay under ignored `run-output/`. For the persistent run, the
runner selects only the assistant JSONL row whose text code units exactly match
the fixed CLI result and preserves that original row line byte-for-byte under
`native-baseline/persisted-assistant-rows/`. The receipt records the raw-row
hash, extracted UTF-16 units and whether the persisted assistant text contains
the Native `API Error: ` prefix. This lets transcript storage and final CLI
result be assessed separately without checking in full sessions or reserializing
the captured row.

The same runner then reuses the nested-lone-surrogate body for 401, 403, 404,
413 and 500 controls with `ANTHROPIC_MAX_RETRIES=0`. `status-controls.json`
fixes the statuses, model, prompt and session IDs. Native completed 403, 404
and 413: 403 used one POST and adds “Failed to authenticate.” before the
`API Error:` text, 404 returns a selected-model access notice, and 413 returns
a request-too-large notice; 404 used two POSTs, the others one each. The 404
notice is observed only for the fixture's fixed `claude-sonnet-4-5` model.
Without a response retry-control header, Native made seven loopback POSTs and
reached the 45-second harness timeout for 401 and 500; that default terminal
path remains unverified.

A separate bounded control adds only `x-should-retry: false` and does not set
the retry-count environment variable:

    CLAUDE_NATIVE_BINARY=/path/to/claude python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/provider-error/run-retry-header-controls.py

Under that explicit header, Native 2.1.293 sends one POST for each status and
returns exit 1 with `is_error: true`, `terminal_reason: api_error`, and the
matching status. 401 shows the authentication notice before `API Error: 401`;
500 shows `API Error: 500` plus the server-side notice. Both final JSON results
retain the nested U+D800. Exact stdout/stderr samples and the control receipt
are in `native-baseline/retry-header-controls/`. The 500 notice embeds a
loopback port; the captured authority is `127.0.0.1:57288`. The orchestrator
candidate fixture injects a fixed `Transport` while using that authority as
the configured request URL, so it never connects to the endpoint. The checked-in
raw output is evidence for that run and is not a cross-run byte oracle. This
covers only the explicit header control and does not establish default retry
behavior.

Static Native binary offsets for the 500 gateway classification are listed in
`NATIVE-SOURCE-EVIDENCE.md`. They explain the runtime observation but are not
execution evidence.

To explain the 404's two requests without saving account-bearing request
metadata into the repository, run the separate bounded capture:

    CLAUDE_NATIVE_BINARY=/path/to/claude python3 /path/to/harness-runtime/docs/parity/current-alignment/fixtures/provider-error/capture-404-request-attempts.py

It stores complete raw request bodies only under ignored `run-output/` and
prints a hash/constraint receipt. The observed chain keeps the same selected
model and changes `stream:true` to `stream:false`; the only parsed JSON diff
path is `/stream`. The status fixture records this semantic chain so Harness
acceptance does not treat the request count alone as a retry contract.

Fresh raw stdout, stderr and the exact JSON stdout bytes are written under
run-output/. The JSON file is a byte-for-byte copy of stdout, not parsed and
reserialized. The receipt's decoded error text and UTF-16 code-unit list are
interpretation metadata; use the raw files for byte comparisons.

The checked-in native-baseline/ contains the captured raw CLI output only
after the run is checked for user-global instruction/account data. Harness
replay must drive the current llm-client Anthropic route and Agent error
delivery path through the ProviderErrorText source wrapper and Host error
formatter; constructing the carrier directly does not close this gate.

On the captured 2.1.293 run, the nested lone-surrogate message appears in the
JSON result as “API Error: 400 fixture provider error U+D800 tail”. JSON parsing
recovers the actual U+D800 code unit; raw stdout JSON contains the escaped
\ud800 sequence and no UTF-8 replacement bytes. An empty body emits “API Error:
400 status code (no body)”. A null nested message falls back to status plus the
complete canonical body. All three JSON outputs report type=result,
subtype=success, is_error=true, terminal_reason=api_error and status 400, with
CLI exit 1 and one POST.

The raw JSON also contains a Native-generated UUID distinct from the fixed
session ID. The receipt checks its format without retaining the identifier as
metadata; the raw artifact remains unchanged. Do not claim cross-run byte
equality for the complete JSON object.
