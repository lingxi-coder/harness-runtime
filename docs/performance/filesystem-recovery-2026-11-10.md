# Filesystem validation recovery — 2026-11-10

The SDK mismatch and all three baseline failures are resolved. Full validation passed in the integrated main workspace, with concurrent changes preserved.

## Fixes

| Problem | Resolution | Evidence |
|---|---|---|
| Runtime used `AnthropicRequestPolicy.plain_text_message_content_indices`, absent from its SDK pin | Added per-message scalar provenance to SDK request policy and pinned `c5d46c5118e3a8fc2950c6b114cb9af0c9db8f85`. The compatibility merge retains the newer SDK revision `b14c43a3` and its Decision changes. | 38 SDK request-policy tests; 3 runtime scalar-provenance tests |
| Mobile app-path fixture expected `.atlas/.apps` | Updated the fixture and its foreign-account rejection case to the SDK's canonical `.atlas-workspaces/apps` layout. Production path policy is unchanged. | Selected-profile test and full platform-common suite pass |
| HTTP timeout fixture expected a call to remain pending | Assert the existing matching `-32001` transport error, then verify later requests succeed before and after the slow handler settles. | Timeout/reuse test and all MCP HTTP tests pass |
| Darwin reported `EPERM` when SIGTERM left only an unreaped zombie | Inspect group liveness on permission denial. Only confirmed absence of live members settles the stop; live or uninspectable groups retain their error. Test groups now use unique markers and retain cleanup ownership. | Real grandchild termination test and live-group denial regression pass |

SDK change: [compatibility commit](https://github.com/lingxi-coder/llm-client/commit/c5d46c5118e3a8fc2950c6b114cb9af0c9db8f85) on `codex/filesystem-sdk-compat-20261110`. Relative to `b14c43a3`, only `src/providers/anthropic/request_policy.rs` changes. Arrays, assistant blocks, cache markers, and exact UTF-16 sidecars retain their source shape. No registry package version was changed by the pin update.

## Validation

| Check | Passed | Failed | Intentionally ignored by ordinary run |
|---|---:|---:|---:|
| Full default filesystem/support suites | 3997 | 0 | 5 |
| File tools with all image/PDF features | 486 | 0 | 2 |
| SDK request policy | 38 | 0 | 0 |
| Integrated runtime scalar provenance | 3 | 0 | 0 |
| Native Claude Code 2.1.322 cases | 403 | 0 | 0 |
| Explicit scaling probes | 3 | 0 | 0 |

The default command includes `tool-file`, `tool-api`, `permission`, `session`, `platform-common`, `platform-posix`, `platform-posix-minimal`, `rooted-fs`, and `telemetry`, including their integration tests and doctests. Native checks verify 64 pinned ranges and include 18 bundled-ripgrep searches. Three scaling probes were invoked explicitly; metadata allocation was measured separately. Existing terminal-multiplexer and subprocess-helper ignore annotations remain; owning PDF tests invoke their child helper explicitly.

All validation ran against `/Users/luolingfeng/lingxi/harness-runtime`, using `--offline --locked` and a dedicated target directory. Workspace crate artifacts were cleared before the acceptance run to avoid stale relative-path metadata from an isolated checkout. Critical source hashes remained unchanged through validation. Initial partial-build logs are retained separately and are not acceptance evidence.

The allocation/scaling checks also retain the optimization results: 256 metadata lookups on a 4 MiB entry allocate zero bytes, versus 1,073,741,824 bytes for full-content lookups; notebook mutation/rendering outputs remain byte-identical to the reference implementations. Raw observations are in `artifacts/filesystem-recovery-20261110/outputs/performance-probes.json`; timings are local observations, not production-latency guarantees.

## Delivery and scope

The runtime fixes were committed in the isolated `codex/filesystem-recovery-20261110` branch and applied as a narrow patch to the main workspace. Backup/simulation hashes match the landed files, the reverse patch check passes, and the main index was unchanged by landing. Current SDK work was retained through the compatibility merge rather than replaced by the older revision.

Detailed target results, commands, source hashes, and landing receipts are in `artifacts/filesystem-recovery-20261110/outputs/verification.json`. The later [filesystem gap follow-up](filesystem-gaps-2026-11-10.md) implements the deferred mutation-I/O offload and documented implementation gaps and records its own validation. This recovery run itself does not add Windows, physical-device, packaged-app, or production/soak acceptance.
