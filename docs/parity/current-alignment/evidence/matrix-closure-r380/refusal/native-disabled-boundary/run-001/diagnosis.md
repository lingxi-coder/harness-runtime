# run-001 launcher failure interpretation

`receipt.json` preserves the unmodified one-shot attempt. Its `native_exit_code`
field reflects the spawned `/usr/bin/sandbox-exec` process; it is **not** an
exit code from the Native CLI. The wrapper exited 65 before `exec` because the
SBPL host token used `127.0.0.1:58084`, which this macOS sandbox parser rejects.
The original stderr is preserved in `capture/native.stderr.raw`; it names the
rejected rule. No Native request arrived, no session JSONL was created, and
both marker files are empty. This run yields no Native tool/refusal semantics.

The revised run-002 script uses the accepted SBPL spelling
`localhost:<port>` with the same listener/base URL at `127.0.0.1`. The separate
`preflight/true.json` receipt records that exact profile form executing only
`/usr/bin/true` successfully, without network activity. run-001 remains intact
and will not be repeated.
