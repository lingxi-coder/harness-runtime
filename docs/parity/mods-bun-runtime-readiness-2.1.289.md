# Bun runtime readiness against Native 2.1.289

The frozen audit is `/tmp/mods-289-bun-runtime-audit/report.md`, SHA-256
`f94f45133259245df01d05a570032f308b2b1840e23f037df465aa2bd244529b`.
Its original bytes and offline probe artifacts are retained. This note adds
current wiring evidence and clarifies topology without editing that snapshot.

Product already supplies `LINGXI_MOD_BUN_EXECUTABLE` for its packaged compiler.
Harness source preparation passed no explicit executable and ignored that
environment setting. The compiler now selects explicit argument, host setting,
then PATH. V288 compiled the Hooks library. Its first filtered run and V290 used the
source filename rather than the actual module name and executed zero tests;
they are retained as diagnostics, not acceptance. V292 used the correct module
path and passed the isolated real TSX/TS regression with PATH unavailable and
the configured executable supplied, then all8 source-preparation tests. The
isolated test is included in those8 tests. All976 source inputs stayed frozen,
and the runner now rejects a successful command that executes zero tests.
Receipts and exact hashes are in the companion JSON.

This does not replace the Node worker or Electron. Installed Bun1.3.11 passes
basic prepared-JS Hook/Client VM probes but lacks the worker's imported
`stripTypeScriptTypes`; a top-level-await SourceTextModule crashes during link
with OS signal5. Native embeds1.4.3, whose equivalent runtime/compiler probes
remain unverified. Native `new Worker` plus shared buffers proves a Worker
object, not a separate OS process. Host child-process isolation is its own
implementation boundary. Native respawn rebuilds modules; current Harness
worker death handling does not yet establish that lifecycle parity.

Private compiler/helper identity, runtime migration, crash/rebuild, real native
and live-provider behavior, visible Product behavior and signed packaging are
open full-goal requirements. The focused compiler-path tests establish only
that the current packaged path controls source compilation.


The fresh runtime-only escape follow-up is saved separately at
`/tmp/mods-289-bun-runtime-followup-v289/followup.md`. Although official Bun
documents `BUN_BE_BUN`, the pinned binary still returned the Claude version
under that setting. No compiler or VM probe followed, so this does not expose
the private embedded runtime or change its unverified status.
