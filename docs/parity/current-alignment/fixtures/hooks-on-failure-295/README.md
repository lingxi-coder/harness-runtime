# Native 2.1.295 classic hook `onFailure: "block"` probes

This fixture compares the default failure handling with `onFailure: "block"`
for three bounded `UserPromptSubmit` failures: a command that cannot spawn, a
command that exits 1, and an HTTP hook that returns 503. Each run is one
sequential CLI process capped at 45 seconds. The hook settings and probe inputs
are isolated under each run directory. All HTTP traffic is loopback-only; the
Messages endpoint uses a fixed dummy key; the HTTP-hook case returns a fixed
local 503. Command stdout/stderr, hook stdin capture, a command marker, Native
stdout/stderr, HTTP request bodies, and response bodies are retained.

`onFailure` is placed on the individual command/HTTP hook handler object. The
Native 2.1.295 schema accepts it there for both handler types. This is a probe
of observed behavior, not an assertion that source/schema evidence alone proves
the result.

Run one case at a time:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude-2.1.295 \
python3 docs/parity/current-alignment/fixtures/hooks-on-failure-295/run-native.py \
  --case command-exit-1-default
```

The runner pins 2.1.295 version and SHA. Fresh raw outputs go to ignored
`run-output/<case>/`; the six captured outputs are curated under
`native-baseline/2.1.295/<case>/`, excluding Native-generated config/cache data.
Observed behavior is summarized in `observations.md`. No 2.1.293/294 baseline
is modified.
