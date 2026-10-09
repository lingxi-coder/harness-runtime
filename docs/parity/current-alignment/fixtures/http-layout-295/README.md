# Native 2.1.295 headless print-text HTTP capture

This is a separate final-target capture. The 2.1.293 Native baseline, its raw
header/body artifacts, and its receipts remain unchanged. The run reuses the
primary headless Native runner from Harness commit
`1d7e55259a16c476eba652a7252f152009862d30`, selecting only `print-text` and
using two Native repetitions. The fixture preserves the original argv, dummy
API key, deterministic SSE response, isolated state/config, raw request line,
ordered HTTP/1 header bytes, complete body bytes, offered/actual response,
stdout/stderr, and runner report.

The helper archives only the exact runner and local helper source files from
that commit into a temporary directory. It changes only the temporary baseline
identity fields to the pinned 2.1.295 binary SHA; no source or existing 2.1.293
artifact is edited. It does not download dependencies or contact an upstream
service.

Run from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.295 \
python3 docs/parity/current-alignment/fixtures/http-layout-295/run-native-295.py
```

The output directory is ignored `run-output/`. Curated raw Native captures and
their receipt are under `native-baseline/2.1.295/`. The request bodies are kept
as captured bytes; comparisons use only the observed billing-version value and
the existing nested `metadata.user_id.device_id` identity rule. No request body
is reserialized and no other field is normalized.

## Captured 2.1.295 result

Both Native 2.1.295 print-text processes exited 0, each made exactly one
loopback Messages request, both returned the fixed 24-byte stdout text, and
both had empty stderr. The 2.1.295 Native-native raw comparison is still
unequal because `metadata.user_id.device_id` changes across isolated runs and
session JSONL timestamps/length differ. The full primary-runner report remains
in ignored `run-output/`; the curated comparison receipt records the bounded
result and raw A/B request files remain available.

The Native 2.1.295 HTTP/1 header block is 935 bytes and stable byte-for-byte
across its two runs. Compared with the preserved 2.1.293 raw header, the only
changed lines are `User-Agent` (2.1.293 to 2.1.295) and the loopback `Host`
port (2.1.293 capture used 54086; this run used 58855). All other header lines,
including `anthropic-beta`, remain raw-byte equal.

Both request bodies are 978 bytes. Their top-level key order and all message,
tool/catalog, token-budget, thinking, context-management, output-config and
stream fields match. Two literal source ranges differ between the versions:
the system billing string changes from `cc_version=2.1.293.7c5` to
`cc_version=2.1.295.fa6`, and the nested `device_id` value changes while
remaining 64 lowercase hex characters. `account_uuid` remains empty, and both
the nested `session_id` and `X-Claude-Code-Session-Id` remain the fixed
`6b7ff52d-e492-4084-88e2-5ad153e3c053`. `native-comparison-293-vs-295.json`
records the exact changed ranges and raw comparison boundaries without copying
the generated device-ID values into the receipt.

The two fixed offered Native responses are 862 bytes and identical to the
2.1.293 offered response. This is a Native-only 2.1.295 capture; the integrated
ApiService/HttpTransport candidate has not been run against this new target.
