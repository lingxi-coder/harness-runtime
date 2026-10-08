# Production Mods worker IPC codec replay

These files are the post-codec Bun 1.3.11 direct-worker run from
`run-harness-worker.mjs`. The runner used the production `mods_worker.mjs` and
shared `crates/hooks/src/mods_ipc_codec.mjs` for every stdin/stdout line. The
dispatch and lifecycle frames are copied byte-for-byte from worker stdout;
`worker-comparison.json` retains the raw result-object slices and Native
comparisons. The separate Native plugin baseline was not modified.

The runner exits 0. Rewrite, cancel/deny, and throw result objects match Native
bytes at the worker dispatch seam. Direct return still differs from Native's
`$.session.append` final result; AgentTranscriptWriter consumption remains the
queued Cargo gate.
