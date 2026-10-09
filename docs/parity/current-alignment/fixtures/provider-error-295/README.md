# Frozen 2.1.295 provider error replay

`run-native.py` runs only the SHA-pinned 2.1.295 executable against an isolated
loopback HTTP server with a dummy API key, empty project/config/home directories,
and the existing fixed `provider-error/cases.json` input. It creates a fresh
ignored run directory and never overwrites a baseline. Session persistence is
enabled so the same invocation also captures the actual assistant JSONL row.

The checked-in baseline is `native-baseline/2.1.295/`. Eight Native invocations
finished with the expected error exit code 1. The three HTTP 400 body cases and
403/413 each sent one streaming request. HTTP 404 sent the same model first with
`stream:true`, then with `stream:false`. HTTP 401/500 used the explicit response
header `x-should-retry:false` and each sent one request. The uncontrolled 401/500
retry lifecycle is outside this probe.

`comparison-to-293.json` compares untouched JSON `result` string literal bytes
from the two versions. Seven are byte-identical. The 500 result differs only in
the actual loopback gateway port; its authority is checked against the listener
receipt. All eight persisted assistant text values contain the same UTF-16
units as their respective CLI result, including the isolated U+D800 fixture.
Full CLI envelopes and complete JSONL rows contain generated fields and are
not reported as byte-equal.

Raw requests, responses, stdout, stderr and assistant rows are retained, with
`receipt.json` and `SHA256SUMS`. This evidence validates reuse of the prior
error-text expectations against the frozen target. Candidate replay is the
separate `provider_error_output_293_test` gate; this Native-only run does not
establish that the candidate passes it.
