# Workspace and session controlled replay: Claude Code 2.1.307

The oracle reads the installed artifact without starting the CLI, extracts 79
bounded source ranges, verifies every range digest, and executes the unchanged
helpers in Node VM contexts with explicit filesystem and host stubs. It imports
only Node built-ins. No credentials, network, or model calls are used, and no
extracted proprietary source is saved in the repository.

- Native binary SHA-256: `5b113a7752119470f1da419cb0e608af3361cae25b614216b13022093958fb2a`.
- [Runner](../../scripts/tests/workspace_session_307_oracle.mjs).
- [Generated fixture](../../crates/session/tests/fixtures/workspace_session_2_1_307.json), schema version 1.
- Fixture SHA-256: `1b69a55ee56cc652958507e648006292dfc94f3cef9d88852bd7f6d4e43c7e9e`.
- The fixture's `sourceByteRanges` records the exact native byte offsets and SHA-256 values.

Run from the repository root:

```sh
node scripts/tests/workspace_session_307_oracle.mjs --check
```

An optional positional argument selects another local copy of the same pinned
binary. Without `--check`, the runner regenerates the fixture. `--check` replays
the cases and compares the complete generated JSON bytes with the saved file.
Both generation and the final check passed with 353 cases:

| Fixture group | Cases |
| --- | ---: |
| Project path encoding | 11 |
| Alias write / read | 43 / 16 |
| Session identity deduplication | 7 |
| Current and repository discovery | 29 |
| Message presence / UUID job helper | 16 / 25 |
| Long slug transcript validation | 17 |
| Resume entrypoint selection | 10 |
| Add-dir validator and text caller | 19 |
| Cd glob / source roots / rooted match | 49 / 15 / 14 |
| Cd permission selection / rejection text | 11 / 26 |
| Directory move / private `set_cwd` control | 13 / 23 |
| Raw `/cd` caller | 9 |

The alias replay executes `OPr`, `W0e`, `tAt`, and `Cuo`. Adding B while A owns
the transcript writes A's complete project-storage path followed by LF into
B's `.session-aliases`; it does not create the reverse link. Existing exact
lines suppress duplicate writes. Reads split only on LF, drop only empty
strings, and preserve first-seen order, CR, whitespace, and unterminated lines.
Appending does not repair a pre-existing missing terminal LF. Same-source,
disabled persistence, diskless, filesystem error, and controlled backend error
branches are included, with exact text bytes, modes, and call traces.
Successful realpath applies NFC; every realpath failure preserves the original
input spelling. The missing and denied NFD cases use the raw `-workspace-cafe-`
bucket, rather than the successful NFC `-workspace-caf-` bucket.

The add-dir replay executes `xwe`, `Pwe`, the text caller, `g7t`, `UDn`, and
`OPr` together. Current cwd, cwd children, and existing additional-directory
grants return before alias recording. A cwd child may register its skills,
commands, and agents without granting access or writing an alias. The native
read fence excludes project-settings additional directories while retaining
user-settings grants. Successful additions write the one-way alias; stat,
network, NUL, and unavailable-project failures do not. Stat, canonical and
network probes, permission mutation, feature gates, and empty hook results are
controlled dependencies. Interactive confirmation UI is outside this replay.

Separate static inspection establishes prompt-root lifetime and reduced-mode
behavior. Registered roots live in the host extensions configuration; `/cd`
clears project/session caches without clearing that collection. A full host
owner reset clears it. The add-dir child gate can enable skills/commands and
agents independently. Safe mode disables both; bare mode permits explicitly
requested skills while disabling agents. The skill loader's bare branch reads
only added `SKILL.md` directories and returns before legacy command loading.
In normal mode, legacy commands are collected in managed, user, project, then
additional-root order; exact-name command lookup chooses the first match.
These observations do not extend the VM's controlled watcher/loader coverage.

`CCo`, `LAt`, `KBe`, and `G9` execute against declared directory inventories.
Aliases expand one hop. Repository aliases come from the current directory and
its nearest supplied containing root. Equal UUID modification times preserve
the first encountered copy; sorting then uses modification and creation time.
The fixture also preserves native per-bucket limits and the absence of a final
repository-wide limit. A separate `Kbn`/`V_e`/`ne`/`Ynt`/`Pue` replay reads the
actual 65,536-byte head and tail slices, prefers the last usable relocation,
and validates the full untruncated slug against recorded cwd metadata.
The physical-mtime cases retain ignored logical-time samples for disk-test
comparison: physical time 100 wins over 90 even when their supplied logical
times are 10 and 80. These cases do not replay metadata enrichment.

The UUID job/helper replay executes `Iue`, `Pe`, `H`, `M`, `f3r`, `rRe`, and `te`: linked scan
candidate, current project, supplied repository worktrees, then one unique
global candidate. Two usable global candidates are rejected. UUID spelling is
preserved. Message presence uses native text-substring checks, including its
different handling of unknown reads in scoped versus global candidates; it is
not a JSON transcript parser.

That helper is not the CLI flag or slash-command entrypoint. A separate group
executes `FIe`, `iQo`, and `fQo` selection, stopping at `beforeUse` before full
transcript processing, and executes the actual interactive `De` caller against
native `LAt`/`G9` discovery. The flag selects the current full-log result before
repository and global fallback; `/resume UUID` first selects the newest matching
repository-catalog entry, including direct aliases. The flag's full-log reader,
repository bucket candidates, and global candidate are controlled, as is the
slash caller's dispatch to `LAt`. Supplied worktree order is preserved; actual
Git enumeration is not tested. Spaced/malformed transcript samples in this
group are paired with explicit controlled-reader outcomes and do not establish
native parser acceptance. A thrown local-reader error propagates rather than
entering repository fallback.

The Cd replay executes native `x`/`B`/`N`, source-root selection, `Jpr`, `WAt`,
and `Byi`. It captures both the plain base notice and the model-visible system
reminder, including `requestBuilt` and fresh-session behavior. The Rust
[matcher and rejection formatter](../../crates/command-api/src/cd_permissions.rs)
and [move notice formatter](../../crates/command-api/src/cd.rs) consume those
fixture groups in tests. The matcher preserves JavaScript's UTF-16 and
case-insensitive, non-Unicode regex behavior, literal punctuation, nonempty
single-star matches, and the distinct double-star forms.

The controlled `set_cwd` cases cover busy/invalid requests, hidden-path
rejection, exact trust-directory echo, `needs_trust`, same-directory no-op,
permission wording, races, notices, and fresh-session settlement. Raw network
classification, path validation results, trust predicates, move side effects,
watchers, and memory are supplied dependencies. Those cases establish the
native branch and response contract, not real filesystem movement or trust
persistence.

`WAt` starts with `transcriptRelocated=true`. Transcript-move failure normally
rolls cwd back and throws. Only a transcript-move failure followed by rollback
chdir failure completes with `transcriptRelocated=false`; fresh-session and
request-built flags do not change it. The raw `/cd` caller supplies neither
fresh-session nor request-built callbacks, so it uses the full stale-environment
notice even when the controlled host reports a fresh session. Its same-cwd
branch prints `Already in ...` without moving or adding a model notice.

Static caller inspection of the same pinned binary establishes the private
SDK field wiring separately from the VM replay: `requestBuilt` captures whether
the queue's dequeue count was greater than zero when the control request was
handled. Freshness requires zero dequeues, no initial user/assistant transcript,
and no current assistant message. It is not inferred from token counts.

| Static caller observation | Native byte range | SHA-256 |
| --- | --- | --- |
| SDK `set_cwd` dependency wiring | `[255187876,255188688)` | `3b47067bc568a56d228081dc8990ae66cbca871770af98063dc5d824cb41e091` |
| Fresh-session predicate | `[255153711,255153778)` | `fefdee24f9951f52fbd2e728740304319bf03bcfcd73ff17fc25768548eeb0f8` |
| Registered-root host owner | `[216954617,216956220)` | `c0f005e2ed2ea5afeacd8b8a0f7f1df13fc90170acbe6951835a5ca7b2175f03` |
| Registered-root getter/setter | `[217010337,217010510)` | `ccd7cf52c7705e295c43a108fc4483cba3898ce97d27f3706c044d92d1f0fba1` |
| Generic markdown collection order | `[226879112,226880967)` | `0bf8447de3a32bce3e6db8cce2e031559c1817eabbb55b75170da247ac73ed5b` |
| Legacy command collection | `[229376306,229377519)` | `a34b49c1fe86a6dfa29f1ea47d240ca4815d500d8e52314e5ad89e8918f1285f` |
| Skill loader and bare early return | `[229378752,229382059)` | `4dd5f6134c7e5a9f6a5f44e0fed7da9fcaaa0b6169f480fbe845e5abcddcfccc` |
| Exact-name command lookup | `[224567662,224567881)` | `499ff44dac50724ae28f3f675a8c765b90c66f77bf2d6b9cf825bdeb0474c3f5` |

This evidence does not establish full CLI parity, actual Git worktree
membership, the All-project picker, storage V5
UUID/long-slug lookup, SDK attachment, Desktop remount, or end-to-end resume.
Header collision classification/recovery, storage validators, and display
escaping are also controlled dependencies. Cargo and application checks belong
to the integration lane and are not claimed by this oracle report.
