# Native 2.1.295 in-flight cancellation

Pinned binary SHA: `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.

The public print-JSON CLI made one streaming dummy-key localhost Messages request. After the server accepted the request, the runner sent SIGINT without returning a model response. Native exited without forced cleanup, emitted `aborted_streaming`, and persisted the prompt followed by `[Request interrupted by user]`. All session-bearing JSONL records use the fixed bare UUID.

The two original content slices match the historical 2.1.293 slices byte for byte. Original user rows and stdout/stderr are retained here. Generated UUIDs, timestamps, durations, cwd and full envelope values are constraints, not byte-equality claims. Full raw request/session artifacts remain under the ignored run directory recorded in the receipt, with hashes. This capture does not establish candidate parity; the real Orchestrator cancellation gate must pass separately.

The runner reads request `stream` from its raw JSON body. The historical 2.1.293 runner inferred that field from Accept headers; its old false value is not evidence of a non-stream request. Historical artifacts remain unchanged.
