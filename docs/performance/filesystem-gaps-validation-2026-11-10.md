# Filesystem validation — 2026-11-10

Publication follow-up: see [the current-main delivery report](filesystem-main-delivery-2026-11-10.md) for the published SDK, clean integration checks, remaining current-main MCP failures and preservation boundary.

All identified code and integration gaps pass the bounded validation below. The canonical checkout preserves concurrent edits; evidence is tied to the frozen runtime at `/Users/luolingfeng/lingxi/.worktrees/filesystem-validated-20261110`. The source graph uses one SDK revision, `c5d46c5118e3a8fc2950c6b114cb9af0c9db8f85`.

| Check | Result |
|---|---|
| Nine filesystem packages, default features | 4,115 passed, 0 failed, 5 intentionally ignored; 146 test targets including integration and docs. |
| tool-file, all features | 548 passed, 0 failed, 2 intentionally ignored; 9 targets. |
| Runtime and replay matrix | Instruction announcements, exact source counts, scalar projection, Read room admission, clipping, lossless resume, compaction, memory contexts, Mod ordering/retry/cancellation, goal usage and thinking recovery passed. |
| Teammate integration | 4 focused tests passed. Captured session/cwd selects transcript storage and hook ownership at actual spawn. |
| Read account integration | 8 Read-count tests and 25 provider-count tests passed. Named accounts work; explicit account revocation cannot select a sibling; unsupported routes do not dispatch; a 401 records the exact identity/revision. |
| Read dispatch integration | 4 host/dispatch tests passed. Missing or stale inherited allowances are rebuilt before overlay; a real Read preserves all 120,001 UTF-16 units and invokes the exact counter once, including a valid route with unknown capacity. |
| Optimization probes | 4 passed: string-array validation, notebook merge, notebook mutation and zero-content-clone metadata lookup. |
| Current production CLI | All four flows passed on the same fresh source build: six-tool lifecycle, malformed UTF-16 lifecycle, ASCII large Read and malformed UTF-16 large Read. Both lifecycle flows resumed with Edit before another Read. |
| Windows GNU cross-check | rooted-fs, tool-file, tool-api, permission and platform-windows passed cargo check. |
| Source checks | 67 scoped Rust files pass full rustfmt; four files retain unchanged baseline formatting and pass parsing. Python parsing and scoped diff whitespace checks passed. |

The final full matrix captured 2,967 source hashes with no drift during execution. All Rust and Cargo inputs remain unchanged afterward, and the fresh production CLI uses that same runtime. A subsequent reporting-only fixture correction changed its computed estimate from ceil to native Math.round; the two count probes were rerun. Execution inputs and the byte-identical native request comparison are unchanged. Earlier attempts and the runtime root-cause trace are retained separately.

The final unmodified CLI binary is `/Users/luolingfeng/lingxi/artifacts/filesystem-gaps-20261110/work/target/debug/lingxi-cli`.
SHA-256: `4865fe8ddbe8ca2d21fe9f93ba050727079f56c436fba97ebdb8e15e249b571d`. Build input hashes stayed stable; no private lockfile updates occurred.

## Native and current CLI evidence

The native reference is Claude Code 2.1.322 at `/Users/luolingfeng/.local/share/claude/versions/2.1.322`, SHA-256 `a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450`. Eight embedded JavaScript oracles passed, including actual bundled-ripgrep execution. Native real CLI lifecycle, direct resume, malformed UTF-16 and count fixtures also passed. Unavailable deep-analysis providers are not presented as evidence.

The two current large-Read probes each emitted one minimal model/messages/tools count request and returned the complete file. Both exact source-unit streams match the native fixtures; 2/2 raw count request bodies are byte identical. The 120,001-unit UTF-16 source retains its single D800, strips the BOM for counting, and has no replacement character in the count payload. Generation applies the native U+FFFD projection. Disk bytes stay unchanged.

The actual CLI trace confirmed the remaining cause: a valid Anthropic route had no catalog context ceiling, so Read returned before attaching its exact counter. Counting now works independently of permission to expand a Read. Unknown capacity still preserves the ordinary byte limit, unavailable guidance and zero lifted-room claims. `read-count-runtime-root-cause.json` retains the failing trace and build; `read-count-counter-only-repair.json` records the correction.

Each LingXi CLI flow creates and removes one disposable dummy named account through public CLI commands inside an explicit task-owned configuration. The adjacent task-only credential-client mock supports only that fixture; the production binary is unchanged. No user credentials, user conversation, external account registration or paid model is used. Startup metadata attempts are denied by the local proxy and recorded separately from provider routes.

Primary receipts live under `/Users/luolingfeng/lingxi/artifacts/filesystem-gaps-20261110/outputs`:

- `final-verification.json`: aggregate evidence and source boundary.
- `verification.json`: full Rust matrix and immutable start/end hashes.
- `current-cli-build.json` and `current-cli-validation.json`: final build and all four actual CLI flows.
- `native-current-count-comparison.json`: native/current exact body and source-unit comparison.
- `native-replay-receipts.json`: pinned native oracle evidence.
- `read-count-account-verification.json`, `tasks-seam-verification.json`: focused integration checks.
- `windows-cross-check.json`, `optimization-probes.json`, `final-source-checks.json`: additional checks.
- `landing-receipt.json` and narrow mirror receipts: scoped landing and preservation evidence. The initial landing preserved 8,131 outside-scope file hashes and the index; later concurrent changes are recorded separately.

## Acceptance boundary

This is filesystem/runtime source and bounded real-CLI acceptance on macOS, using the validated snapshot. Windows cross-compilation does not execute reparse-point, UNC, file-lock or cancellation behavior on Windows. Native Windows execution, signed Desktop broker/native keychain, packaged UI, physical devices, live provider tokenizer equality and long-duration production soak were not exercised. No CI workflow was dispatched and no product commit or push was made by this follow-up.
