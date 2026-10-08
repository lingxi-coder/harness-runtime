# Statusless Anthropic stream-error Native oracle

This independent Native 2.1.293 fixture sends an HTTP 200 SSE body containing
`event: error` and a JSON `data` object with `type: error`. The primary case
uses nested `error.type: timeout_error` and a `message` JSON escape for one
isolated UTF-16 high surrogate; the control case uses `api_error`. Inputs and
expected UTF-16 units are fixed in `cases.json`.

Run with an explicitly selected pinned Native binary:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude python3 docs/parity/current-alignment/fixtures/statusless-stream-error/run-native.py
```

The runner sets `ANTHROPIC_MAX_RETRIES=0`, a dummy API key, project-only
settings, isolated HOME/config, and a 127.0.0.1-only loopback server. It checks
that the first request asks for a stream and records every physical request's
stream flag, raw SSE body, exact Native stdout/stderr, final JSON fields, and
the selected result's UTF-16 units. It does not infer the final text from an
SDK intermediate. The result distinguishes a preserved lone surrogate, the
literal characters `\\ud800`, and U+FFFD. Any stream-to-nonstream fallback is
reported explicitly.

On Native 2.1.293, every case made one `stream:true` request followed by one
`stream:false` recovery request despite `ANTHROPIC_MAX_RETRIES=0`. The no-start
error reports zero stream events; the partial case reports three events
received, but both final JSON results contain only the generic HTTP 200
event-stream fallback diagnostic. Neither includes the nested message,
partial text, U+D800, literal `\\ud800`, or U+FFFD. This final CLI output does
not expose or settle the SDK's intermediate `makeMessage` string handling.

The initial run is an oracle only; the aligned llm-client test and the outer
Host formatter remain separate candidate gates.
