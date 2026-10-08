# Observed WebFetch output parser behavior

Reference: Native `2.1.293 (Claude Code)`, SHA-256
`4e21122a227857da1178aca3299700c1fd7f2b77c93f12e73c2c76db796a105e`.
The run used a fixed local model stub, dummy key, isolated HOME/config/cwd,
`--tools default,WebFetch`, and the fixed resource URL
`http://127.0.0.1:41237/fixture/<case>`. Each case made exactly two accepted
loopback Messages POSTs; no external model or WebFetch destination was
configured. The exact model responses, Native session rows, result JSON slices,
model-facing tool-result content, hook markers, stdout/stderr and hashes are
retained beside this note. Full request bodies remain in ignored `run-output/`.

The public `tool.call` event has top-level `url`, `prompt`, `tool`, and
`tool_use_id` fields. The hook awaited `next(event)` and then returned a fixed
WebFetch result. Native's next result for the `http://127.0.0.1` URL was
`Error: error:100000f7:SSL routines:OPENSSL_internal:WRONG_VERSION_NUMBER`;
the local HTTP resource server received no request. The hook replacement still
reached Native's output parser, so the results below concern the parser seam,
not a successful WebFetch network operation.

For `valid-unknown-keys`, the hook result had unknown keys both at the top
level and inside `artifactRead`. Native's exact `toolUseResult` JSON omits all
three unknown fields and retains the known values in schema order:

```json
{"bytes":24,"code":200,"codeText":"200 OK","result":"fixed loopback WebFetch result","durationMs":1,"url":"http://127.0.0.1:41237/fixture/valid-unknown-keys","artifactRead":{"slug":"fixture-artifact","ver":"v1","seeded":false}}
```

The negative cases produce these exact `toolUseResult` strings:

```text
seeded: true or "false" -> Error: tool.call step resolved WebFetch with a result that does not match its output shape: [
  {
    "code": "invalid_value",
    "values": [
      false
    ],
    "path": [
      "artifactRead",
      "seeded"
    ],
    "message": "Invalid input: expected false"
  }
]
numeric slug -> Error: tool.call step resolved WebFetch with a result that does not match its output shape: [
  {
    "expected": "string",
    "code": "invalid_type",
    "path": [
      "artifactRead",
      "slug"
    ],
    "message": "Invalid input: expected string, received number"
  }
]
missing slug -> Error: tool.call step resolved WebFetch with a result that does not match its output shape: [
  {
    "expected": "string",
    "code": "invalid_type",
    "path": [
      "artifactRead",
      "slug"
    ],
    "message": "Invalid input: expected string, received undefined"
  }
]
```

These are Native-only parser observations. They do not establish a candidate
Harness parser/worker/Host match.
