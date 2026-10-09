# Native 2.1.295 observations

Reference binary: `/Users/luolingfeng/.local/share/claude/versions/2.1.295`

Version: `2.1.295 (Claude Code)`

SHA-256: `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`

All six isolated `--print --output-format json` invocations completed within
the 45-second cap, with Native process exit code 0. Each `onFailure: "block"`
case ran the hook, returned a JSON `result` envelope with `is_error: false`,
`num_turns: 0`, and `stop_reason: null`, and made zero Anthropic Messages
requests. The result text identifies `UserPromptSubmit operation blocked by
hook` and includes the failing hook detail. This is a successful CLI process
whose prompt turn was blocked, not a process-level failure.

| Failure | `onFailure` omitted | `onFailure: "block"` | Hook evidence |
| --- | --- | --- | --- |
| Command cannot spawn (`ENOENT`) | 1 Messages request; one model turn | 0 Messages requests; 0 turns; result contains `posix_spawn` error | No command stdin or marker because spawn failed |
| Command exits 1 | 1 Messages request; one model turn | 0 Messages requests; 0 turns; result contains `FIXED_COMMAND_HOOK_STDERR` | Hook ran in both; stdout marker was not surfaced; stderr marker appears in the blocked result only |
| HTTP hook returns 503 | 1 HTTP hook request followed by 1 Messages request; one model turn | 1 HTTP hook request and 0 Messages requests; 0 turns; result contains `HTTP 503` | Fixed local 503 response; no command side effects |

The block policy therefore changes whether the model request is dispatched for
these three `UserPromptSubmit` failures. In each paired case, the omitted-field
control continues after the same failure. The raw JSON output, command stdin,
HTTP bodies, responses, and receipts are preserved byte-for-byte in
`native-baseline/2.1.295/`; receipts retain run-specific session IDs, loopback
ports, request counts, and SHA-256 values. Re-run captures are kept separately
in ignored `run-output/`.

This evidence covers only classic `UserPromptSubmit` command spawn failure,
command exit 1, and HTTP 503. It does not establish semantics for other hook
events, exit statuses, transport failures, asynchronous hooks, or other Native
versions.
