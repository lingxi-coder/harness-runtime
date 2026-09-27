# Claude Code 2.1.270 cron oracle

Source: official `@anthropic-ai/claude-code-darwin-arm64` 2.1.270 package binary
`package/claude`, SHA-256
`a506b6d970a4cf44f6abdb53a81ddcd5d3b0ce042a95c502fe9d1f946bdb8807`.
The verifier reads extracted chunks from that binary; upstream source is not
vendored here.

Run:

```sh
python3 tools/cron/tests/oracle/verify_latest.py /tmp/lingxi-loop-oracle-2.1.270
cargo test -p cron -p tool-cron --lib
```

The directory argument must contain `package/claude` and `chunks/*.js`. The
verifier checks the binary hash before evaluating its pure functions through
Node.js. It uses no upstream network or credentials.

Evidence anchors:

- `src_168489904.js`: `aIn(true)` CronCreate prompt, default seven-day expiry.
  The only substitution is `.claude/` → `.lingxi/`. Result SHA-256:
  `2e0d7f90941dc3159a9bdd6de9bae75e508f22ca3d5e2ca0b92477a8006ed2f6`.
  It matches the existing `cron_create_prompt_2_1_263.txt` fixture byte-for-byte.
- `src_168472338.js`: `JO` parser, `Nje` calendar search, `GF` jitter defaults,
  durable creator metadata. The search limit counts calendar advances, not
  elapsed minutes. The verifier covers JavaScript whitespace/large steps and leap-day search plus spring/fall DST;
  `cron::schedule::tests::latest_oracle_calendar_jumps_and_dst` locks the same
  output instants with an injected timezone offset.
- `src_185261434.js`: CronCreate schema, validation, result text and durability.
- `src_169577058.js`: `G4` converts only exact string `true` and `false`.
- `src_195250200.js`: `Pe` expires recurring, non-permanent jobs after their
  final due fire; zero max age disables expiration. Creator session/PID/start
  identity controls whether a live foreign session may run a durable job.

The compiled default jitter is 50% with a 30-minute cap even though the upstream
prompt says 10% / 15 minutes. Preserve both upstream surfaces independently.
Session cron uses `.claude/scheduled_tasks.json` and the project-wide
`.claude/scheduled_tasks.lock` leader lease. Fires enter the owning conversation
as Later/meta input; there is no production Dream-agent fallback. The independent
v2 task center retains `.lingxi` storage and its own mutation lock. No legacy
store fallback or migration is performed.

`verify_storage.py` executes the upstream persistence functions with deterministic
host seams and validates `session_storage_2_1_270.json` byte-for-byte (SHA-256
`504b7fd2b9ff1b628a6237c7b7aa371170b246e2e24caf905ef192d8d99a3da6`).
These fixtures establish the compiled-default path; they do not establish parity
with server-delivered `tengu_kairos_cron_config` cohorts, which need an object-valued
feature configuration transport that this repository does not currently expose.
