# Native built-in Glob `tool.call` output-parser probe

This bounded fixture checks whether Native 2.1.293's public plugin `tool.call`
hook can intercept the built-in `Glob` tool and supply a controlled output to
its parser. The loopback-only Messages stub returns one fixed `Glob` tool call;
the isolated working directory contains only `probe.fixture`. The plugin returns
a fixed legal-shaped Glob result with a deliberately nonexistent filename
(`hook-return.fixture`) and writes a marker if the hook executes. The persisted
`toolUseResult` and next model request reveal whether that return replaces the
built-in result.

Run from the Harness repository root with the pinned binary:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
python3 docs/parity/current-alignment/fixtures/tool-output-parser/run-native.py
```

The runner pins the Native SHA, uses a dummy key and an isolated HOME/config,
sets `ANTHROPIC_MAX_RETRIES=0`, and binds its model stub only to loopback. It
defaults to `--tools default,Glob` and uses `--dangerously-skip-permissions`
for the fixed read-only Glob call. To record the default CLI profile that omits
Glob, run:

```sh
CLAUDE_NATIVE_BINARY=/path/to/claude \
CLAUDE_NATIVE_TOOLS=default \
NATIVE_BUILTIN_RUN_NAME=glob-default-only \
python3 docs/parity/current-alignment/fixtures/tool-output-parser/run-native.py
```

Full requests stay in ignored `run-output/`; each run receipt hashes the exact
first-request `tools` slice and selected durable result rows. Selected raw
artifacts are copied to `native-baseline/2.1.293/` without full provider request
bodies.

`--tools default` does not advertise Glob in this Native CLI profile. The
explicit `--tools default,Glob` profile advertises only Glob; treat the two
catalogs as separate fixed inputs, not as evidence of global tool removal.

`NATIVE_BUILTIN_CASE` accepts `override`, `unknown-top`, `duration-1e400`,
`duration-infinity`, `duration-negative-infinity`, and `duration-nan`. The first
proves the continuation wrapper replaces the built-in result; the second records
top-level unknown-key stripping; the numeric cases test finite-value rejection.
`duration-wrong-type`, `duration-missing`, and `filenames-item-wrong-type` record
the Native issue path and message for shape errors. `known-reversed` submits all
known fields in reverse order and saves the exact raw `toolUseResult` JSON to
expose Native's output key order without reserializing it.

The `override` case uses a unique hook-returned filename to prove the returned
wrapper reaches Native's parsed `toolUseResult`. `unknown-top` adds an unknown
top-level object. Native strips that field. `duration-1e400`,
`duration-infinity`, `duration-negative-infinity`, and `duration-nan` are
rejected as non-number values with the exact native diagnostic retained in their
raw model result files. Glob has no known nested object field, so nested
unknown-key stripping remains unverified.
