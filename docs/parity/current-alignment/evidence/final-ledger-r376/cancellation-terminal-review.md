# Independent cancellation terminal review — r10 final

Reviewed H r10 commit `44f2eef5baf8ae16254934dbca451cbfd528d955` (parent
`b3864c6ca6c16ed682a333ad96f7ad59cd05fb0b`) and the Product test/fixture delta on
`8610f0535e719537e953ea553de3196f40a821ba`. **No confirmed source blocker
remains for the known pending-POST cancellation case.** The r9 positional-index
issue is fixed in r10 by passing the current query's MessageId through the
existing public streaming API and looking up that ID after the turn.

The focused r10 H runtime gate reports **1,468 passed, 0 failed**; fmt and
changed-source clippy exited 0 in `harness-repair-gates-r10`. I did not run
Cargo or Native. Product's public CLI test and four fixture files are unchanged
from the prior review.

## Unverified boundaries — not allowed differences

If an installed `session.compact` Mod changes compact rows, the compaction
handler can regenerate IDs on both the summary and preserved tail. The r10
classifier then cannot find the original query ID and falls back to the old
success projection. That implementation behavior is source-confirmed, but its
Native parity impact is **unverified**, not a confirmed source bug: the Native
query-abort path shows `aborted_streaming` and the plain user interruption row,
but the Native print serializer for this Mod-renumbered history is unavailable.
Do not infer the output envelope or `num_turns`, and do not mark this case as an
allowed difference. The r10 publisher test covers a compaction replacement
that retains the query ID; it does not cover the Mod-renumbered case.

The pre-request boundary also remains unverified. Cancellation before turn
admission leaves no prompt row with this ID, so the classifier does not match.
A later loop-top cancellation after prompt admission but before model dispatch
can append the plain interruption row and match the ID. The Native fixture
demonstrates only SIGINT after the loopback Messages POST was accepted; it does
not establish the print envelope for the post-admission/pre-request case.

The positive r10 publisher test first places older history, the query, and the
interruption in the session, then replaces the history with a compact summary
plus the same query ID and interruption. It verifies the Native error frame.
The adjacent partial-assistant and tool-use interruption tests still retain the
success projection. Product's `--output-format json` integration test routes
through the shared `StreamJsonStream` one-shot publisher; the multi-turn
`--input-format stream-json` loop still uses the generic publisher and remains
outside that one-shot boundary.

The Product integration test waits for the single local Messages POST, sends
SIGINT while the stub withholds the response, and checks selected stable result
fields and two raw transcript content tokens. Its three raw payload hashes and
2.1.295 binary binding are unchanged; it does not claim whole-output byte
equality. The 60-second loop bounds child-exit observation and has child/stub
cleanup; `wait_with_output()` itself is not timed, so I found no concrete
deadlock in this no-tools fixture path but do not claim an unconditional bound
on a hypothetical descendant-held-pipe failure.

## Reviewed inputs

- H HEAD: `44f2eef5baf8ae16254934dbca451cbfd528d955` (parent `b3864c6ca6c16ed682a333ad96f7ad59cd05fb0b`)
- H `run.rs` working-tree SHA-256: `f9afe39ea99eaffa8afb3097b53bfab8aa17ae13f3e28788a40aca768f30b8ee`
- H target diff SHA-256: `0c24a988c61843714e5afaba51de859ec0efc60c2b52e491592cf041594ffd00`
- Product HEAD: `8610f0535e719537e953ea553de3196f40a821ba`
- Product `commands_native_cli.rs` SHA-256: `af62c3656a9d8b9b833c7b4766909c4a95d34ba9846bbdb885d53d9eda0ab1f0`
- Product test diff SHA-256: `5948eddc84d4eb55fdb269663eeaaf2bd0e2814a8896615650e16a69b8cf4e91`
- Native fixture hashes remain recorded in the Product test and
  `tests/fixtures/cli-cancel-native-295/README.md`.
- Previous reports are preserved as
  `cancellation-terminal-review-r9.{md,json}` and
  `cancellation-terminal-review-r10-initial.{md,json}`.

**Conclusion: r10 closes the old history-index defect for the captured
ID-preserving path. The Mod-renumbered and post-admission/pre-request Native
print projections remain unverified, not accepted differences.**
