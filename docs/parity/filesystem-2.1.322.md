# File tools and filesystem contracts: Claude Code 2.1.322

The [official npm latest metadata](https://registry.npmjs.org/@anthropic-ai/claude-code/latest)
reported **2.1.322** on November 9, 2026 (America/Los_Angeles). REA verified
the installed Darwin arm64 executable at
`~/.local/share/claude/versions/2.1.322`, SHA-256
`a9d237949db816a0b7afd88a9a8e1df36b3a597a30f5a41d1c0539eb180a3450`.

This change compares finite file-tool contracts against that executable.
Oracle runners verify the executable and every extracted source range before
executing unchanged native helpers in controlled Node VM contexts. Fixtures
retain inputs, expected results and range receipts; proprietary source stays
outside the repository. Search fixtures additionally run the executable's
bundled ripgrep applet over disposable files. No account, model request or
user file is used by these runners.

## Changes

| Surface | Aligned behavior |
| --- | --- |
| Paths | ECMAScript whitespace trimming, home expansion, NFC normalization, lexical resolution, null-byte errors and unstable whitespace detection before filesystem access. Permission canonicalization remains a separate step. |
| Read | Native newline/range handling, UTF-16 token estimates and truncation, encoding notes, fresh rereads, normalized notebook routing and the `allow_large` schema/budget gates. |
| Large Read accounting | Host-resolved provider and compaction budgets; shared claims across a query's parallel tool calls; atomic refreshed-cap admission before publication. Hosts without a trusted budget retain the native unavailable branch. |
| Edit/Write | Native 8,000-byte encoding classification, UTF-16LE/BOM preservation, CRLF handling and its 4,096-code-unit classification sample, Unicode escape transformation, partial-view write guards and file-type errors. |
| Glob/Grep | Incomplete-result notices and timeout metadata; UTF-16 long-line cutoff; content/context separators; single-file and multiline rendering; NUL/binary search summaries; repeated directory-prefix handling. |
| NotebookEdit | Hard validation failures, exact cell-source joining, metadata language, native cell IDs/results, one-space ECMAScript JSON serialization including unpaired surrogates and number/key ordering, native size guards and strict stale-read handling. |

Notebook insert IDs use the first eight characters of a fresh UUID. They do
not truncate invocation IDs, whose timestamp prefix can repeat. Notebook
serialization retains the source JSON projection while mutating cells, so
unrelated properties and exact UTF-16 strings survive the edit.

## Reproduction

From this checkout:

```sh
node scripts/tests/file_read_322_oracle.mjs --check
node scripts/tests/file_mutation_322_oracle.mjs --check
node scripts/tests/file_search_322_oracle.mjs --check
node scripts/tests/file_notebook_path_322_oracle.mjs --check
cargo test -p tool-file -p tool-api --offline --no-fail-fast
cargo test -p orchestrator --lib read_context_room_host_tests --offline
```

Each runner accepts one explicit path to the pinned executable when it is
installed elsewhere. The search runner invokes only `ARGV0=rg` on disposable
fixture paths; it does not launch an assistant session.

## Boundaries

These are helper replays and Rust filesystem regressions, not a full native
CLI end-to-end acceptance or cross-platform equivalence claim. POSIX path
fixtures do not establish Windows drive/UNC semantics. LingXi's rooted I/O,
permission checks, guest-path translation and provider support remain product
adaptations.

Large Read uses LingXi's authoritative live context accounting and existing
provider routing. Equivalence with Claude Code's separate history estimator
and live `count_tokens` service is not established. A default-size read can
still proceed after contextual room shrinks: that is the native fallback,
not a guarantee that all results fit a hard context quota. Remote calls and
hosts without budget state do not receive a larger allowance.

Native memory `startOnly` tracking is a separate persistent fence, including
file-identity alias checks. LingXi does not currently model its memory-clipping
producer; this patch does not infer it from transformed memory content.
Explicit partial Read views are protected. Malformed UTF-16LE disk text with
unpaired surrogates still has Rust replacement-character decoding; exact
preservation of such disk text is outside the accepted mutation cases.

Native Glob may return a lower-bound total after postfiltering active Read-deny
rules, whereas LingXi reports the exact permitted count. Search deadlines are
enforced between files; interrupting a pathological match within one file and
full ripgrep type/collation equivalence are not established. Image/PDF codecs,
OCR, native filesystem transactions, platform adapters and full CLI resume
receive no new acceptance claim from these finite text/search cases.
