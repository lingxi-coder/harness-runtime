# Orchestrator contracts: Claude Code 2.1.319

Reference artifact: Claude Code 2.1.319, SHA-256
`1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
SDK revision: `3cb37e3a3b840b54a4e92994fdc8b1edf7f592d3`.

The alignment covers observed request and message contracts:

- Native request property ordering, including reserved properties subsequently
  supplied by extra-body overrides.
- Tool-use/result repair in original call order, exact UTF-16 identities,
  original thinking neighbors and source-preserving synthetic results.
- Tool-result persistence limits measured in UTF-8 bytes, UTF-16 size reporting,
  preview boundaries, JavaScript whitespace and ordinary write-failure recovery.
- Native media admission after normalization and pairing, with route-specific
  count, byte and document limits.
- Two malformed-tool repairs per tool round, independent of output-limit and
  thinking-only recovery; exhaustion publishes its specialized API error.
- Opt-in, physical-attempt-scoped raw partial frames alongside SDK semantic
  decoding, execution and accounting.
- Frozen refusal facts, validated response identifiers, typed API-error rows,
  independent outer/inner UUIDs and stable durable/public projections.
- Final NDJSON escaping of U+0085, U+2028 and U+2029. The JSONL fixture contains
  three complete native stdout records and must retain their original bytes.

`scripts/tests/*_319_oracle.mjs` contains eight orchestrator-specific checks:
`tool_result_persistence`, `tool_result_pairing`, `request_layout`,
`malformed_tool_use`, `media_limits`, `refusal_api_text`, `partial_stream` and
`synthetic_api_error_identity`. Run a check with the exact reference binary:

```sh
node scripts/tests/request_layout_319_oracle.mjs /path/to/claude-2.1.319 --check
```

Rust coverage includes the related orchestrator and llm-runtime unit tests,
session serialization, core refusal helpers, headless stream-json output, and
the `current_alignment_tool_result_persist` and `malformed_tool_use_319_test`
integration targets.

Unsafe tool-result identifiers deliberately use opaque, safe filenames.
External schema references and custom metaschemas bypass compiled-schema reuse
so changes to referenced resources are observed.

This is bounded contract parity. The SDK rejects nonfinite `1e400` metadata
with raw capture both enabled and disabled. Private diskless storage, unsent
object identity, private host facts and server experiments are not synthesized.
LingXi branding, directories and supported capabilities remain host-owned.
Real-provider, device, Windows and long-running acceptance are separate from
the source-oracle and local regression evidence.
