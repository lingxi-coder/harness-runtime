# Native 2.1.293 Glob output-parser evidence

These selected raw artifacts come from the public CLI runner in the parent
fixture directory. Native SHA-256 is
`4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`.
The CLI used an isolated HOME/config/cwd, a fixed loopback-only Messages stub,
and a dummy API key. Full Messages request bodies remain in ignored
`run-output/` because they contain Native system context; these copies retain
the exact tool catalog, stub SSE, hook marker, model-facing tool result, selected
session rows, and each run receipt.

`glob-default-only/` records that `--tools default` does not advertise Glob.
The explicit `--tools default,Glob` cases advertise only Glob and exercise the
same public `tool.call` continuation wrapper. A direct handler return did not
replace the native result. Returning `{...nativeResult, result, text}` after
`await next(event)` did; the unique hook filename appears in both the parsed
`toolUseResult` and the next model request.

`glob-unknown-top/` shows Native removes an unknown top-level object. The Glob
schema has no known nested object field. The `glob-duration-*` cases preserve
the full native parser errors: `1e400` evaluates to JS Infinity, and positive
Infinity, negative Infinity, and NaN are rejected where a number is required.
`glob-duration-wrong-type/`, `glob-duration-missing/`, and
`glob-filenames-item-wrong-type/` each record one native issue, including the
exact path and message. `glob-known-reversed/` preserves the exact raw
`toolUseResult` JSON: despite reversed input field insertion, Native returns
`durationMs,numFiles,filenames,truncated,totalMatches,countIsComplete` order.
The Native session-row UUID and timing values are one-run evidence; only the
controlled tool input/result path and parser behavior are assertions.
