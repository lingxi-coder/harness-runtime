# Claude Code 2.1.286 non-text tool-result fixtures

`generate.mjs` executes the official `b5e`, `Fgn`, `$gn`, `re`, and `Q2e`
functions from extracted 2.1.286 source. `S` is the upstream JSON.stringify
wrapper with telemetry omitted; `fM` is its 50,000 UTF-16-unit limit. The real
surrogate-safe truncator and code-point counter are executed, including the
boundary that would otherwise cut an emoji in half.

Binary SHA-256:
`75e3016e9d2570767b08e43a7467d4817a4f149232c169ca295f2c95fef21433`.
Chunks: `src_183727495.js` (normalization) and `src_176129258.js` (text utilities).

```sh
node crates/orchestrator/tests/fixtures/tool-result-2.1.286/generate.mjs /tmp/claude-code-oracle-2.1.286
```

The test checks model-facing content and preservation of the raw transcript.
It does not claim whole-provider-wire parity or live-provider validation.
