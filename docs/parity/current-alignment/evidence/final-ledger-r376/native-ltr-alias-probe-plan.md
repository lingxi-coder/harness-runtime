# Native 2.1.295 alias/glob probe — review before one run

This plan resolves only the remaining Native `LTr` glob line-terminator uncertainty. The pinned public input path is confirmed in `source-alignment-295-r31`: `native-policy-source/src_186935115.js` (`tBn`, SHA-256 `4ffda2309ae77a82b6f16bee91efbcee75c960f69d6a45638f9cc574e1a186a1`) declares optional string-to-string `toolAliases` on the `initialize` control request; `session-state-storage-source/src_221001555.js` (SHA-256 `9a69afbfbbfb568b80e2b8a9424db6e278f5420f939dee65b67ab3c88801c05a`) copies it into options and rebuilds `toolPermissionContext`. No `--tool-aliases` CLI flag was found.

The runner is [`native-ltr-alias-probe-once.py`](native-ltr-alias-probe-once.py). It is prepared but has **not** been executed. After review, invoke it once with `CLAUDE_NATIVE_BINARY` pointing to the 2.1.295 executable. It checks SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70` without launching a separate `--version` process. The output directory is a one-shot latch: a second invocation refuses to start another Native process. No Cargo or Product build is involved.

The single process uses `--print --input-format stream-json --output-format stream-json --verbose`, fixed model `claude-sonnet-4-5`, fixed session UUID, `--max-turns 1`, and `--tools Read,Bash,Glob,Grep,Write,Edit,NotebookEdit`. It receives one initialize control request, waits for its matching success acknowledgement, then receives one prompt requesting a tool-name listing without a tool call. An isolated project settings file supplies these deny patterns:

| Case | `toolAliases` source → canonical tool | Settings deny pattern | What presence tests |
| --- | --- | --- | --- |
| LF internal | `lf_a\nb` → `Read` | `lf_a*b` | Whether `*` spans an internal LF |
| CR internal | `cr_a\rb` → `Bash` | `cr_a*b` | Whether `*` spans an internal CR |
| LS internal | `ls_a\u2028b` → `Glob` | `ls_a*b` | Whether `*` spans U+2028 |
| PS internal | `ps_a\u2029b` → `Grep` | `ps_a*b` | Whether `*` spans U+2029 |
| Suffix control | `suffix_ab\n` → `Write` | `suffix_a*b` | Terminal-newline behavior; `$` may match before a final line terminator, so this is not an internal-newline test |
| Plain positive control | `pos_ab` → `Edit` | `pos_a*b` | Confirms a non-newline alias deny is active |
| Presence control | no alias → `NotebookEdit` | none | Confirms the requested catalog reached the provider request |

The runner sends a fixed, local Anthropic SSE text response (`OK`) with no tool call, and allows one provider POST. It records the exact initialize and user frames, initialize acknowledgement, provider request and response, CLI stdout/stderr, session JSONL when present, tool-name observations, hashes, and the fixed inputs. `Edit` must be absent and `NotebookEdit` present; otherwise the receipt is `capture_invalid` and no alias/glob inference is valid. The other six catalog-presence observations are retained individually for interpretation.

Isolation follows the existing `cli-cancel-295/run-native.py` posture: isolated temporary `HOME`, `CLAUDE_CONFIG_DIR`, cwd and tmp; a dummy API key; no inherited environment or proxy; telemetry/nonessential traffic disabled; and only the configured provider endpoint is bound to `127.0.0.1`. This does not install an OS-level egress sandbox. The one-run limit is 60 seconds; on timeout or any failed precondition the runner stops and records failure, with no retry.

This probe observes only provider-visible tool-name presence for this fixed request. It can bind these specific Native permission/catalog outcomes, but it cannot establish complete raw request/session byte parity, every regex construct, or live provider behavior. The unavailable `LTr` body remains an explicit source limitation; any outcome should update only the observed cases.

Review adjustment: the operational deadline is 56 seconds, reserving up to four seconds for termination inside the 60-second process bound. Per-tool outcomes remain catalog observations rather than independently calibrated removal causes.
