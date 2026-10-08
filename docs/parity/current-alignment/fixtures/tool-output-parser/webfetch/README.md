# Native 2.1.293 WebFetch nested output parser fixture

This public CLI fixture sends a fixed `WebFetch` tool call to a `tool.call`
plugin hook. The hook awaits `next(event)` for a fixed loopback-only HTTP URL,
then returns one controlled output object to Native's WebFetch result parser.
On 2.1.293, `next(event)` returns an SSL `WRONG_VERSION_NUMBER` error before
the local page server receives a request; the public hook still reaches the
Native output parser with the replacement result. The fixture covers a valid
`artifactRead` with unknown fields, `seeded: true`, `seeded: "false"`, a
non-string `slug`, and a missing `slug`. Two separate follow-up cases combine a
missing top-level `bytes` with nested invalid fields and pass explicit
`artifactRead: null`.

Run from the Harness repository root:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/tool-output-parser/webfetch/run-native.py
```

The Native binary is pinned by version and SHA-256. Both the Anthropic Messages
stub and WebFetch page bind to `127.0.0.1`; the fixture uses a dummy key, an
isolated config/HOME/cwd and disables telemetry. The resource URL uses the
fixed port in `cases.json`, so an occupied port is an explicit fixture setup
failure. Run receipts preserve request/response hashes, exact extracted
catalog/result JSON slices, session JSONL rows, hook marker, stdout and stderr.
Full model requests stay in ignored `run-output/`; the checked-in Native
baseline keeps raw response SSE, hook markers, result JSON/session rows,
model-facing result content and stdout/stderr under
`native-baseline/2.1.293/webfetch-nested/`.
The follow-up receipt and two new cases are under
`native-baseline/2.1.293/webfetch-nested-followup/`. To rerun only those cases,
set `NATIVE_WEBFETCH_RUN_NAME=webfetch-nested-followup` and
`NATIVE_WEBFETCH_CASES=bytes-missing-slug-number-seeded-true,artifact-read-null`;
the run-name option keeps ignored outputs separate from the original baseline.

These observations cover Native parser behavior at the public tool-call hook
seam only. They do not establish a full Harness worker/Host pass.
