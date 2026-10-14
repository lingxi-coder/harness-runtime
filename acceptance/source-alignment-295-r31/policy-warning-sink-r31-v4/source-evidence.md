# Native policy warning rendering v4

This is an immutable detached increment based on the v3 applied source hashes in `manifest.json`. It changes only the existing Harness pinned-Bun worker/asset, the existing policy-helper warning call path, and the Product CLI warning sink. It does not modify Core warning data or add a new worker process.

The fixed Claude Code 2.1.295 source is `native-nl-source/src_186240709.js`, SHA-256 `e3196d36258c99d66c4d3ea2401745eae6e9a94e148380f6ff0fe1f4723043bf`.

- Native debug-filter functions `Jn/Yn/Xn/Ot`, bytes `[4953,6006)`, SHA-256 `d6fee11520c553afb8e404228e87946e86413d6142815e75c21e56ed972f3e60`. The worker uses these functions on the original UTF-16 message and the CLI filter string before any redaction.
- `D.redact` class and its local helpers, bytes `[6006,34006)`, SHA-256 `505c725f218082e51085c4709850e949aeae14e2ec84811a226804a040e89cfd`.
- The local `Je` rule predicate required by `D.redact`, bytes `[44351,44444)`, SHA-256 `c2bde4f27969f79ff0aa6c718c947d07bed9579c5ce125d0d7bb040d5b9c0c24`.
- Native log-level and invisible-character regex `ki`, bytes `[44853,44951)`, SHA-256 `efdc7ca65818a10a5b52fad71dff1dfe694f55caa1b6dab9db960477fe9bf54f`.
- `Sn.log`, bytes `[48111,48625)`, SHA-256 `3a01c5afad3a17501731d4ce56a62fd389f06686e1767e199013a08f3382b272`, applies the configured minimum level before `emit`.
- `Sn.emit`, bytes `[48625,48856)`, SHA-256 `eae9fc70082306df5775d49757fe59d50f42d4c21b52e10d3eece91b75573e9d`, performs `shouldLog`, trims, calls `D.redact`, applies `vH(JSON.stringify(...))` for multiline or `replace(ki,t4)` for single-line, then writes an ISO timestamp, `[WARN]`, and one final LF.
- `_` at bytes `[57146,57230)`, SHA-256 `8b98c8266718d1d2a57c3775a2c4d1796908a4b211e39e522c5ac6fc1fdc3cf1`, is a telemetry-wrapped `JSON.stringify`; the returned JSON string is the native JSON serialization used in multiline framing.

The worker implements those slices inside the already registered Bun semantic worker. It receives the warnings and debug-filter string as UTF-16 code-unit arrays, returns an optional exact rendered line per input in the same order, and validates the echoed input units. Product writes only the already-rendered line; strict UTF-16-to-UTF-8 conversion occurs at the physical file/stderr boundary, with no second timestamp, escaping, or newline insertion. `isAnt` remains an independent false CLI baseline; provider branding is not used to infer it.

The existing callsites remain in place: first on-chain static warnings are emitted at the initial/local source-change step, and helper-output warnings are emitted after the helper output is applied. This patch preserves their ordering and adds no off-chain static logging. If the Bun renderer is unavailable, the sink suppresses the warning instead of writing unredacted text; Core retains the source warning rows.

## Pure worker evidence

`worker-input.jsonl` and `worker-output.jsonl` are synthetic and contain no real credentials. The pinned runtime receipt is `/Users/luolingfeng/lingxi/.worktrees/claude-alignment-integration/acceptance/source-alignment-295-r31/bun-1.4.3-oracle-runtime.json`: Bun 1.4.3, binary SHA-256 `28c7dd0a06916aba284d570ef44a2d70e9c538eaf2ba58dfe0910525563b3f79`. The worker ping returned revision `c6da4a4d3010e5553438c60f6bd76d981976867c`, Unicode 16.0, protocol 4, and worker ID `native-policy-js-semantics-2.1.295-v4`.

`verify_worker_output.py` checks four concrete behaviors: synthetic token redaction, debug-filter rejection, multiline D800 escaping while retaining a valid emoji surrogate pair, and control/U+2028 escaping. The direct Bun worker run passed. Rust formatting and patch-application checks passed; Cargo and the Native CLI were not run.
