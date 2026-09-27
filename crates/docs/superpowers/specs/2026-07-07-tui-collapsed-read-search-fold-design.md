# Design: TUI collapsed read/search tool fold (`collapsed_read_search` parity)

**Date:** 2026-07-07
**Status:** Draft — awaiting review
**Scope:** Full 1:1 port of claude-code's `collapsed_read_search` feature into the ratatui TUI.

## 1. Problem

During a turn the assistant often fires a long run of `Read` / `Grep` / `Glob` calls.
Today each one commits as its own 3-line scrollback cell (`⏺ Read {json}` + `⎿ {result}`),
so a dozen reads produce ~36 lines of noise (see the reported screenshot). claude-code
folds a consecutive run of read/search tools into **one** line that updates live:

```
Streaming:   ⏺ Reading 7 files, Searching for 3 patterns          (spinner + … + ctrl+o hint)
               ⎿ …/vulkan_computer/src/main/cpp/computer.cpp        (dim, latest op, active only)
Finalized:   ⏺ Read 7 files, Searched for 3 patterns, Listed 2 directories
```

The Rust port **already has the data model and a render cell** for this
(`RenderedMessage::CollapsedReadSearch` + `history_cell::tool::CollapsedReadSearchCell`),
but the **folding pass that produces it was never implemented** — those variants are only
constructed in tests. `client-adapter/src/lowering.rs` explicitly notes folding "stays
CLIENT-SIDE" and the adapter emits raw `ToolUseStarted`/`ToolUseResult`. The client side
of that contract is missing. This design fills it.

## 2. Reference (authoritative parity source)

| Reference file | Lines | Role |
|---|---|---|
| `utils/collapseReadSearch.ts` | 1110 | classification + whole-array grouping transform + summary text |
| `components/messages/CollapsedReadSearchContent.tsx` | 484 | the collapsed cell renderer (summary parts + hint + verbose entries) |
| `tools/shared/gitOperationTracking.ts` | 278 | `detectGitOperation` — commit SHA / push / branch / PR extraction from bash output |
| `utils/memoryFileDetection.ts` | 290 | `isAutoManagedMemoryFile` / `isMemoryDirectory` / `isAutoManagedMemoryPattern` / `isShellCommandTargetingMemory` |
| per-tool `isSearchOrReadCommand` | — | `FileReadTool`, `GrepTool`, `GlobTool`, `BashTool` (the big one), `PowerShellTool` |

## 3. Behavior (parity contract)

### 3.1 What folds (classification — `getToolSearchOrReadInfo`)

Each tool use is classified, in this precedence:

1. **REPL** (`REPL_TOOL_NAME`) → collapsible, `isAbsorbedSilently` (no count, no summary; its
   inner tool calls flow through as virtual Read/Grep/Bash messages).
2. **Memory write/edit** — `Write`/`Edit` whose `file_path` is an auto-managed memory file → `isMemoryWrite`.
3. **Snip / ToolSearch** — collapsible, `isAbsorbedSilently` (visible in verbose only).
4. **MCP tool** whose underlying tool `isSearchOrReadCommand` → carries `mcpServerName` ("Queried slack").
5. Otherwise delegate to the tool's `isSearchOrReadCommand(input)` → `{ isSearch, isRead, isList }`.
   - `Grep`/`Glob` → search; bash `grep`/`rg`/`find -name` → search.
   - `Read` → read (unique file paths); bash `cat`/`head`/`tail` → read (no path, operation count).
   - bash `ls`/`tree`/`du` → list.
6. **Fullscreen only:** a non-search/read Bash command → `isBash` ("Ran N bash commands"),
   otherwise a bash command breaks the group.

### 3.2 Grouping (`collapseReadSearchGroups`)

A `currentGroup` accumulator absorbs a **contiguous run** of collapsible tool uses + their results.
Category counters: `searchCount`, unique `readFilePaths` (Set), `readOperationCount` (pathless reads),
`listCount`, `replCount`, `mcpCallCount`+`mcpServerNames`, `bashCount` (fullscreen), memory
`recall/search/write`, `latestDisplayHint`, plus git-op accumulators (commits/pushes/branches/prs)
and PreToolUse-hook timing.

**Absorbed (do not break the group):** collapsible tool results; PreToolUse hook summaries
(→ `⎿ Ran N PreToolUse hooks (Xs)`); `relevant_memories` attachments (→ "recalled N memories").

**Deferred (do not break, re-emitted after the group flushes):** skippable messages —
thinking, most attachments, system messages — so the collapsed badge keeps the position of the
first tool use. (Exception: `nested_memory` attachments pass straight through.)

**Break the group (flush → commit, then handle the breaker):**
- assistant text (`isTextBreaker`),
- a non-collapsible tool use,
- a user message with a non-collapsible tool result.

`createCollapsedGroup` finalizes counts: `readCount` = unique non-memory file paths, falling back
to `readOperationCount` when there are no path-based reads (so `Read(x)` + `Bash(wc -l x)` stays 1);
memory counts subtracted out and re-added from absorbed `relevant_memories`.

### 3.3 Display (`CollapsedReadSearchContent`)

One summary line built from ordered parts, comma-joined, first part capitalized:
`[git ops (fullscreen)], [Searched for N patterns], [Read N files], [Listed N directories],
[REPL'd N times], [Queried <server> N times], [Ran N bash commands (fullscreen)],
[Recalled N memories], [searched memories], [Wrote N memories]`.

- **Active** (still streaming): present-continuous verbs (`Reading`, `Searching for`, `Listing`,
  `Querying`, `Recalling`…), a leading spinner, a trailing `…`, then `ctrl+o to expand`.
  A dim `⎿ <latestDisplayHint>` line renders **only while active**.
- **Finalized:** past-tense verbs (`Read`, `Searched for`, `Listed`…), dim, no spinner, no hint line.
- **Verbose (ctrl+o):** expands to the per-absorbed-message rows (each tool-use/result).
- Hook timing line `⎿ Ran N PreToolUse hooks (Xs)` when present.

`getSearchReadSummaryText` produces the plain-text form used by the status/spinner line.

## 4. Architecture in LingXi — streaming accumulator (commit-once adaptation)

**The core mismatch:** claude-code re-runs `collapseReadSearchGroups` as a *pure whole-array
transform on every React render*, so groups form and re-form each frame. The LingXi transcript
commits each cell into **native terminal scrollback exactly once** and cannot un-commit — so we
cannot re-run a batch transform and re-emit committed cells.

**Decision: port the grouping as an incremental streaming accumulator**, not a batch transform.
This yields byte-identical *output* because claude-code's groups are already contiguous runs sealed
by the same breakers; only the computation moves from batch → incremental. It fits the existing
single-active-cell model (`Transcript::set_active`/`mutate_active`/`flush_or_discard_active`): a
group is exactly one `CollapsedReadSearchCell`, which lives in the mutable **active slot** while
streaming and is **committed once** on flush.

Rejected alternative (batch-over-uncommitted-region): keep the message list and re-run the batch
transform over an uncommitted tail each frame. More faithful to the *code* but requires the
transcript to support a re-collapsing multi-cell uncommitted region — a larger transcript
rearchitecture for identical output. Not chosen.

### 4.1 Accumulator placement (`tui/src/chat_widget.rs`)

Introduce a `CollapseGroup` accumulator field on `ChatWidget` (the client-side fold state). Rework
the tool-event arms (`chat_widget.rs:521` `ToolUseStart`, `:548` `ToolUseResult`):

- **Collapsible `ToolUseStart`:** do NOT `push_message` a raw `ToolUseCell`. Instead:
  if no group is open, `flush_or_discard_active()` (seal any streaming text/thinking) and open a
  group in the active slot; feed the classified use into the accumulator; re-render the active
  `CollapsedReadSearchCell` (is_active = true).
- **Collapsible `ToolUseResult`:** update the accumulator (append verbose entry; refine hint;
  fullscreen bash-result git-op scan); mutate the active cell in place. Do **not** commit a result cell.
- **Breaker events** (non-collapsible `ToolUseStart`, assistant `TextDelta`, `ThinkingStart`,
  `TurnEnded`): `finalize_group()` → commit the single `CollapsedReadSearchCell`, replay any
  deferred-skippable cells, then process the breaker as today.
- **Deferred skippables mid-group:** buffered and replayed after the group commits. (Thinking
  rarely interleaves between reads; correctness preserved, live thinking simply waits for the seal.)

### 4.2 Active-cell live render

`CollapsedReadSearchCell` gains an `is_active` flag and an `animation_key` (spinner phase) so the
active group re-renders as reads stream in — reusing the existing spinner infra used by
`ThinkingCell` / activity spinner.

## 5. Data-model changes (`tui-core/src/message.rs`)

Enrich `RenderedMessage::CollapsedReadSearch` (fields already present: `search_count`, `read_count`,
`list_count`, `is_active`, `group_id`, `entries`, `mem_read`, `mem_search`, `mem_write`). Add:

- `latest_hint: Option<String>` (the `⎿` active hint),
- `repl_count: u64`,
- `mcp_call_count: u64`, `mcp_server_names: Vec<String>`,
- `bash_count: u64` (fullscreen),
- `hook_count: u64`, `hook_total_ms: u64`,
- git-op summaries: `commits: Vec<GitCommit>`, `pushes`, `branches`, `prs` (fullscreen).

`entries: Vec<String>` stays the verbose-expansion source. This is a shared-struct field-add — per
project lore it breaks every workspace literal of the variant, so all construction/match sites
(incl. the `tui/src/message.rs` and `history_cell` tests) update in the same commit.

## 6. New Rust modules to port

1. `tui-core` (or a shared util crate) `collapse/classify.rs` — `getToolSearchOrReadInfo` +
   per-tool `is_search_or_read_command` for `Read`/`Grep`/`Glob`/`Bash`/(`PowerShell`).
   The Bash classifier (parse `cat`/`ls`/`grep`/`rg`/`find`/`head`/`tail`/`tree`/`du`…) is the
   largest piece; port `BashTool.isSearchOrReadCommand` faithfully.
2. `collapse/group.rs` — the `CollapseGroup` accumulator (fields per §3.2) + `finalize()`
   (`createCollapsedGroup` count math).
3. `collapse/git_ops.rs` — `detect_git_operation` (port `gitOperationTracking.ts`).
4. `collapse/memory_detect.rs` — port `memoryFileDetection.ts` helpers.
5. Rewrite `history_cell::tool::collapsed_read_search_lines` to the real
   `CollapsedReadSearchContent` format (§3.3), replacing the `Read/Search (N results)` placeholder.

## 7. Feature gating & deferrals (matching claude-code)

- **Fullscreen-gated** (`isFullscreenEnvEnabled()` → LingXi inline TUI = off by default): git-op/PR
  summaries and "Ran N bash commands". Port the logic but gate it behind the equivalent LingXi
  fullscreen check so the default inline experience matches claude-code's default.
- **Feature-flag-gated:** `TEAMMEM` (team-memory counts) and `HISTORY_SNIP` (Snip absorption).
  Port behind the same flags; inert when off.

## 8. Testing

- **Classification unit tests** per tool (Read/Grep/Glob/Bash commands → isSearch/isRead/isList).
- **Grouping unit tests:** contiguous run → one group; text/non-collapsible-tool/non-collapsible-result
  break; hook + relevant_memories absorption; skippable deferral ordering; unique-file dedup +
  bash-read fallback count math.
- **Renderer tests:** active present-tense + spinner + hint + `…`; finalized past-tense no hint;
  verbose entry expansion; each summary part + comma-join + first-capital; memory/mcp parts.
- **Accumulator integration test** in `chat_widget`: a `ToolUseStart×N` + results run yields one
  active cell that finalizes to one committed `CollapsedReadSearch` on a breaker; deferred thinking
  replays after.
- **git_ops / memory_detect** ports get their reference-derived unit tables.
- Full `cargo test -p tui -p tui-core` + workspace `cargo check --tests` green; real iTerm2 smoke
  (the `TestBackend` emits no ANSI, and live-render regressions have historically only shown on a
  real terminal).

## 9. Component inventory (reference → Rust)

| Reference | Rust target |
|---|---|
| `getToolSearchOrReadInfo` + per-tool `isSearchOrReadCommand` | `collapse/classify.rs` |
| `collapseReadSearchGroups` / `createCollapsedGroup` / accumulator | `collapse/group.rs` + `chat_widget.rs` streaming driver |
| `getSearchReadSummaryText` | `collapse/group.rs` (status text) |
| `CollapsedReadSearchContent.tsx` | `history_cell::tool::collapsed_read_search_lines` (rewrite) |
| `gitOperationTracking.ts` | `collapse/git_ops.rs` |
| `memoryFileDetection.ts` | `collapse/memory_detect.rs` |
| `RenderedMessage::CollapsedReadSearch` | enrich `tui-core/src/message.rs` |

## 10. Out of scope

- Non-fold rendering paths (Edit/Write diffs, bash output cells) — unchanged.
- The `GroupedToolUse` variant (`● Read (×N)` same-tool fold) — separate, not produced here.
- Resume/replay reconstruction of collapsed groups from JSONL — the live path first; replay parity
  is a follow-up if the recorded transcript needs the fold too.
