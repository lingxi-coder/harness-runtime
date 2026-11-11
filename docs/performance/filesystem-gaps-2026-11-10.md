# Filesystem gap follow-up — 2026-11-10

The identified filesystem and integration gaps are fixed in the canonical runtime. Validation uses a frozen snapshot because the shared checkout receives concurrent changes. The snapshot includes the dirty workspace baseline; private checkpoints must not be published as product commits.

| Area | Result |
|---|---|
| Mutations | Bounded blocking preparation, serialized owned commits, cancellation settlement, same-opened-handle SHA-256 checks before truncation, exclusive creation, FIFO-safe opens, rooted NotebookEdit, and synchronous history/cache publication. |
| Exact text and replay | Lossless UTF-16 decoding, ranges, matching, edits, diffs, results, errors, notebook sources and IDs, freshness, durable replay and direct resumed Edit. Linear matching avoids quadratic work. |
| Memory and context | Persistent startOnly fences across LRU eviction, aliases, sessions, forks and restore; owned worker context; bounded Read room reservations and compaction publication after admission. |
| Search | Complete results beyond 10,000 records; native lower-bound Glob totals, type catalogue and locale order; bounded admission; supervised system-rg cancellation and malformed argument handling. |
| Permission compilation | Bounded singleflight compilation and lazy active-policy plans; authorization decisions are evaluated live. |
| Tool contracts | Native parser/serializer replay, aggregate nested diagnostics, exact CLI input schema receipts and safe integer bounds. |
| Read token counting | Minimal source-only Anthropic count requests, exact UTF-16, native budget fallback, authorized named-account selection, pinned explicit accounts, and exact rejected credential identity. Valid routes with unknown capacity retain normal-cap counting while larger reads remain unavailable. |
| Integration | Correct SDK revision, transcript resume, dynamic memory projection, selected Mod publication and retry state, goal usage locking, subprocess cleanup, and captured teammate transcript storage. |

Validation passed: 4,115 tests across the nine filesystem packages; 548 tool-file tests with all features; focused runtime/replay tests; four optimization probes; four real CLI flows; and Windows cross-compilation. Final commands, source boundaries and receipts are in [the validation report](filesystem-gaps-validation-2026-11-10.md).

The native boundary is explicit: invocation, disk, durable tool data, resume, and count_tokens preserve lone UTF-16 units; generation HTTP string values use U+FFFD. The compatible SDK applies that generation projection without altering source storage.

CLI fixtures use disposable files, isolated configuration, a dummy named account, a loopback provider, and a task-only mock credential-client dependency. They exercise an unmodified production CLI binary. Signed broker/native keychain, Windows execution, packaged UI, device, live tokenizer and soak acceptance remain unverified.
