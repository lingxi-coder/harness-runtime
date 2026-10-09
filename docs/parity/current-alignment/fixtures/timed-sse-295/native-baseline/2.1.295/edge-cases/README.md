# Native 2.1.295 timing edges: HTTP fallback and retry

These captures use the frozen 2.1.295 binary, fresh isolated HOME/config/cwd, a dummy key, and a loopback-only model endpoint. `manifest.json` indexes the raw request bodies, response bodies/SSE, stdout, stderr, event traces, and per-case receipts. Existing single-stream timing baseline files remain unchanged.

The 404 case returns the captured 2.1.295 provider-error body to the initial streaming request, then a complete Anthropic message JSON body with HTTP 200 to Native's nonstream fallback. Native sent `stream:true` then `stream:false` and completed successfully. Final stdout had `ttft_ms`, `duration_api_ms`, and `duration_ms`, but no `time_to_request_ms`, `ttft_stream_ms`, or `first_content_frame_ms`.

Two 500 cases set `x-should-retry:true`, `retry-after:0`, and `ANTHROPIC_MAX_RETRIES=1`, then return a delayed successful SSE response. Changing only the first 500 response delay from 200 ms to 400 ms moved the observed second request from 989.363 ms to 1235.374 ms after Native spawn, and moved `ttft_stream_ms` from 1204 ms to 1449 ms. `time_to_request_ms` remained 20 ms versus 18 ms. This is consistent with that field retaining the initial request timestamp through retry; it is a scoped inference because the timer's internal origin is not exposed. The raw event and request traces are retained so this inference can be revisited.

The fixed binary SHA is `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. Reproducible case input and runner are under `fixtures/timed-sse-295/edge-cases/`; running them writes only to the ignored `fixtures/timed-sse-295/run-output/` directory.
