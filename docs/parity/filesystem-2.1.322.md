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
| Tool contracts | Latest prompt/description bytes, schema field metadata, mutation output validation and unknown-property stripping; partial Edit results omit the full-context reassurance. |

Notebook insert IDs use the first eight characters of a fresh UUID. They do
not truncate invocation IDs, whose timestamp prefix can repeat. Notebook
serialization retains the source JSON projection while mutating cells, so
unrelated properties and exact UTF-16 strings survive the edit.

## Reference checks

All five native replay runners passed: **403 cases across 64 pinned ranges**.

| Runner | Cases | Source ranges |
| --- | ---: | ---: |
| Read | 207 | 20 |
| Mutation | 47 | 11 |
| Search, including 18 actual bundled-ripgrep searches | 49 | 10 |
| Paths and notebooks | 48 | 6 |
| Prompts, descriptions, schema factories and Edit result messages | 52 | 17 |

## Rust verification

Validation used an isolated checkout merged with runtime main `cba2130f`.
Every command completed with zero failures:

| Check | Passed |
| --- | ---: |
| Tool API suite, including atomic concurrent Read admission | 317 |
| Default file-tool library suite | 417 |
| Unix-socket Write regression, run separately | 1 |
| File-tool library with all image/PDF/render features | 454 |
| Prompt/schema/parser integration tests, in each feature configuration | 4 |
| Main-loop context-budget host tests | 2 |

The socket fixture required running outside the host sandbox and was filtered
from the two larger suite invocations; it passed separately. There were no
ignored tests. Rustfmt and `git diff --check` passed. Cargo ran offline with
incremental compilation and debug information disabled after initial builds
exhausted shared disk space. The main-loop check compiles the orchestrator
test target but executes only the two named budget tests.

## Reproduction

From this checkout:

```sh
node scripts/tests/file_read_322_oracle.mjs --check
node scripts/tests/file_mutation_322_oracle.mjs --check
node scripts/tests/file_search_322_oracle.mjs --check
node scripts/tests/file_notebook_path_322_oracle.mjs --check
node scripts/tests/file_tool_contract_322_oracle.mjs --check
cargo test -p tool-file -p tool-api --offline --no-fail-fast
cargo test -p tool-file --all-features --offline --no-fail-fast
cargo test -p orchestrator --lib read_context_room_host_tests --offline
```

Each runner accepts one explicit path to the pinned executable when it is
installed elsewhere. The search runner invokes only `ARGV0=rg` on disposable
fixture paths; it does not launch an assistant session.

## Follow-up closure and boundaries

The November 10 follow-up implements lossless malformed UTF-16LE Read/Edit/Write and notebook identifiers, persistent memory `startOnly` fences and physical aliases, owned blocking mutation transactions with same-descriptor content revalidation, native Glob postfilter totals, the bundled ripgrep type catalogue, locale collation, and streaming Grep result projection without the former 10,000-record loss. Permission matcher plans retain the active rule set and coalesce concurrent compilation without caching authorization decisions.

Schema coverage now executes the actual bundled Zod validator and serializer. It includes aggregate nested diagnostics and the six input schemas captured from a real native CLI provider request. Model-free native CLI fixtures exercise all six tools, process resume, and the actual Read token-count request. LingXi's host counter pins the provider route and counts the exact Read source; unsupported counters retain the native local fallback. The current-source validation ledger is in [the follow-up report](../performance/filesystem-gaps-2026-11-10.md).

Native optional attached-machine transport and dormant edit-anchor/prefix gates remain disabled. LingXi's rooted I/O, permission checks, guest-path translation, and multi-provider context accounting remain product adaptations. A default-size Read may still proceed after contextual room shrinks; this matches the native fallback and is not a hard context quota. Remote hosts without trusted budget state receive no lifted allowance.

Finite fixture replay and local CLI acceptance do not establish live provider tokenizer equivalence, arbitrary remote-host behavior, production soak, signed Desktop package acceptance, physical-device acceptance, or Windows runtime parity. The available Windows cross compiler can establish compilation only. Individual OS calls, codec stages, and in-process regex operations are not preemptible; checks run between bounded stages/chunks. Multiline matching retains a file-sized search buffer, and explicitly unlimited results retain requested output storage. The explicit desktop Unix system-ripgrep backend owns its process group and deadline; unsupported platform configurations report an error.
