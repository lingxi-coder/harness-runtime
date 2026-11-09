# Plan Mode: Claude Code 2.1.319

The reference is the shipped macOS Claude Code 2.1.319 executable, SHA-256
`1f53587e92dee8e089fa349188a4cda64fa34ace6e7b0ca2ee278ddaa3cc2ebd`.
The release was checked on 2026-11-09 at
<https://github.com/anthropics/claude-code/releases/tag/v2.1.319>.

## Byte contract

`scripts/tests/fixtures/claude_2_1_319_plan.json` records artifact-derived
prompts, input schemas, tool metadata, confirmation text, reminders and Exit
result text. The fixture includes default/custom output styles, hybrid search,
empty custom instructions, empty/BOM/NEL plans, edited plans, Agent availability
and unpaired UTF-16 surrogates. Optional result fields are omitted when absent.
The native executable was also replayed against a loopback API capture for
starting in Plan, entering, repeated entry, approved exit and denied exit.

The Enter search branch follows trusted launch facts, including explicit
Glob/Grep opt-in and Windows shell availability. It is independent of the Auto
permission policy. Output-style changes invalidate the tool-prompt cache.
Enter and Exit retain the reference's ToolSearch deferral, read-only,
concurrency and result-size metadata. Exit's interaction requirement is resolved
for each invocation because a registered teammate review transport differs from
the main session and ordinary agents.

```sh
python3 scripts/tests/plan_319_byte_check.py
node scripts/tests/plan_319_oracle.mjs /path/to/claude-2.1.319 /path/to/native-request.json --check
node scripts/tests/plan_hybrid_319_oracle.mjs /path/to/claude-2.1.319 --check
cargo test -p tool-plan --lib --locked --offline
cargo test -p permission -p orchestrator --lib plan --locked --offline
```

The small byte checker compiles the actual production text functions and
whitespace/environment helpers with a substitute UTF-16 transport container.
Full `tool-plan` tests exercise the production transport, schemas, ToolSearch
wire states, permission gate and session transitions. These checks do not imply
that every host event or every Claude feature is byte-identical.

## Runtime contract

Plan entry updates both the permission gate and session state and is idempotent.
Successful exit restores the pre-Plan permission mode, unless approval selected
another mode. Denial and failed edited-plan saves retain Plan. Auto selection
is committed only after saving and checking that the approval still belongs to
the same session and Plan entry; its availability is checked again at commit.
An ordinary child agent cannot mutate the root permission mode.

`/plan`, the editor, reminders, file-write permission exceptions and Exit all
use one shared plan identity. The slug is persisted in typed transcript
metadata and restored on cold/hot resume. Mobile project activation rebinds the
configured plans directory to the restored project root; a shell `cd` does not
rebase it. Invalid slug metadata cannot redirect the plan outside that directory.

The task's captured evidence and final verification record are under
`../artifacts/plan-mode-319-20261109/` in the parent workspace. Desktop package
and physical-device acceptance are separate from the source-level checks.

The publishing check on isolated sources from `main` passed 31/31 byte
comparisons and 238 tests selected by the `plan` filter: 36 core, 49 runtime,
66 orchestrator, 55 permission and 32 tool tests. Both Desktop and Mobile runtime
features were enabled. This includes persisted plan identity across cold/hot
resume, Mobile project switches and shell working-directory changes.

```sh
cargo test -p core -p permission -p tool-plan -p orchestrator -p harness-runtime --lib plan --features harness-runtime/desktop,harness-runtime/mobile --locked --offline
```
