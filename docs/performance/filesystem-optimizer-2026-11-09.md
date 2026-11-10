# Filesystem optimization audit and implementation — 2026-11-09

## Executive summary

Implemented **29 deduplicated findings: 0 critical, 5 high, 19 medium, 5 low**. The audit covered all 13 code-optimizer domains plus logging, using pattern-first searches and narrow confirmation before implementation. Independent reviews covered permission/state/projection/history semantics and asynchronous Read/search/lock/PDF ownership.

The three largest changes are:

1. Keep Read, contended rooted locks, search cancellation, and PDF processes from blocking the async runtime or retaining resources after cancellation.
2. Remove cached-content copies and repeated notebook/output-tree rebuilding. A 4 MiB cache entry now supports 256 metadata lookups with zero allocated bytes, versus 1 GiB previously.
3. Bound search bookkeeping to the requested output. The 50,000-candidate fixture retains 100 filenames; only-matching line accounting scans each prefix byte once.

## Scope and baseline

Scope is the harness-runtime filesystem subsystem: Read/Edit/Write/NotebookEdit/Glob/Grep, rooted filesystem primitives, POSIX/minimal/Windows adapters, guest path forwarding, read state, file history, permission projections/caches, image/PDF processing, and their telemetry. This is source-code optimization, not host-volume maintenance.

Baseline: `6b42c27332f85181e0d427aef522f261e64a3460`, combining the accepted Claude Code 2.1.322 file-tool alignment with current runtime/SDK corrections. Branch: `codex/filesystem-optimizer-20261109`. Source references below refer to that baseline, so line numbers remain unambiguous after the changes. Baseline copies and detailed logs are retained in `artifacts/filesystem-optimizer-20261109/work/`.

The patch preserves ordering, error selection, UTF-16 JSON behavior, permission checks, descriptor authority, fresh reads, and mutation cancellation ownership. Existing parity limits remain documented in `docs/parity/filesystem-2.1.322.md`.

## Findings by file

All rows below are implemented. Related call sites and shared helpers are included in the patch; each finding is counted once.

### `crates/platforms/posix/src/fs.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F17: 575, 593 | HIGH | Concurrency | Contended rooted flock blocked async runtime threads | Prepare one pinned handle; nonblocking retries with async timer | Current-thread runtimes and single blocking-worker pools make progress |

### `crates/rooted-fs/src/lib.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F27: 3944, 4001 | HIGH | Resilience | Regular-file to FIFO replacement could block opening before cancellation | NONBLOCK plus existing opened-descriptor regular-file checks | Prevents FIFO swap from exhausting Read slots |
| F22: 3343 | MEDIUM | Resilience | Tail reads could chase concurrent appends beyond metadata snapshot | Take at most snapshot length minus start | Bounded suffix bytes despite append growth |

### `crates/tools/file/src/grep.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F18: 1455, 831 | HIGH | Resilience | Per-file searches could ignore deadline/context cancellation and caller drop | Shared stop controls at entries, 64 KiB reads, matches/context | Cooperative timeout/cancel while preserving partial timeout output |
| F08: 1611 | MEDIUM | Complexity | Bounded filename pages retained/sorted all hits | Stable top-k of offset plus effective limit | Bounded requested prefix; same ties and pagination |
| F09: 963, 1521 | MEDIUM | Memory | Content/count formatted and retained every row before pagination | Count all rows; format and retain only requested page | Bounded page retention with full totals |
| F10: 212 | MEDIUM | Complexity | Only-matching output rescanned line prefixes for every span | Monotone newline-prefix cursor and streamed spans | Total prefix scan bounded by input length |

### `crates/tools/file/src/pdf_render.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F28: 179 | HIGH | Security-performance | Page renderer reopened original pathname after validated snapshot | Stage captured bytes inside renderer-owned scratch directory | Renderer consumes the permission-validated snapshot |
| F19: 169, 186 | MEDIUM | Concurrency | PDF collection/page count/image encoding ran inline on async workers | Bounded offload; two-process render cap | Keeps CPU/I/O codec work off async workers |
| F29: 186 | MEDIUM | Resilience | Timeout/drop could delete scratch before child termination was reaped | Supervisor owns child, permit and scratch through termination/cleanup | Cleanup cannot race live renderer handles |

### `crates/tools/file/src/read.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F16: 1358 | HIGH | Concurrency | Synchronous whole/ranged Read I/O occupied Tokio workers until EOF | Four-slot blocking work with owned permit and chunk cancellation | Runtime stays schedulable; bounded active Read/media workers |
| F06: 2586 | MEDIUM | Rendering | Notebook rendering happened before byte/token rejection gates | Render only after existing admission checks | Avoids work for rejected notebooks |
| F25: 2196 | LOW | I/O | Known extension paths sought to zero even without header sniffing | Seek only after an actual header read | Removes redundant seek |

### `crates/permission/src/filesystem.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F11: 1237, 1455 | MEDIUM | Caching | Compiled-pattern caches permanently stopped admitting after 256 entries | Bounded FIFO replacement including negative/exhausted outcomes | New active patterns can hit after saturation |
| F14: 1204 | LOW | Complexity | Every ancestor path was folded again | Fold once and borrow ancestor views | Avoids repeated ancestor allocations |
| F15: 1780 | LOW | Complexity | Character-class markers repeatedly scanned the full source pattern | Lazily precompute occupied markers once | One occupancy scan when classes need it |

### `crates/permission/src/policy_gate.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F13: 3185, 3212 | MEDIUM | Caching | Search prepared live policy and projections repeatedly | Capture coherent exclusion/path-policy bundle once | One policy snapshot per search |

### `crates/permission/src/read_deny_globs.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F12: 209 | MEDIUM | Complexity | Growing Vec.contains dedup scanned all prior projections | IndexSet preserves first-seen order | Expected linear stable dedup |

### `crates/platforms/common/src/guest_fs.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F21: 114 | MEDIUM | I/O | Guest wrapper inherited full-file read for bounded transcript prefix | Resolve read path then delegate bounded prefix | Reads only requested catalog prefix |

### `crates/session/src/file_history.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F23: 511, 630 | MEDIUM | Memory | Each picker/rewind query cloned entire snapshot history | Resolve one owned restore entry per tracked file under lock | Copies scale with selected files instead of snapshot count |

### `crates/tool-api/src/native_output.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F04: 188, 115, 223 | MEDIUM | Data structures | Validation and materialization repeatedly rebuilt full-arena subprojections | Borrow validation; create only requested subtrees; lazily clone passthrough | Removes projection allocation on valid validation-only arrays |

### `crates/tool-api/src/read_file_state.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F01: 202 | MEDIUM | Memory | Metadata callers cloned the entire cached String under the mutex | ReadFileMetadata/get_metadata preserves MRU with scalar copies | Content-size-independent metadata lookup |
| F02: 340 | LOW | Memory | Refreshing an existing entry cloned all stored path aliases | Update occupied entries in place | Preserves alias storage and accounting without refresh copies |

### `crates/tool-api/src/tool_result_media.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F05: 265 | MEDIUM | Data structures | Adjacent notebook text repeatedly copied the full accumulated UTF-16 prefix | Accumulate text once and flush at image/end boundaries | Linear accumulation; exact text/image order |

### `crates/tool-api/src/util/image_budget.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F20: 149 | MEDIUM | Memory | Original compressed image/raster remained live through resize/encode | Drop source once passthrough is ruled out; release original raster | Reduces overlapping image allocations |

### `crates/tools/file/src/edit.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F24: 778, 832 | MEDIUM | Complexity | Stale Edit repeated normalization and match analysis | Reuse actual match/count; avoid quote-matching char buffers | Fewer full-content scans and temporary allocations |

### `crates/tools/file/src/glob.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F07: 728 | MEDIUM | Complexity | All matching filenames were stored and sorted for a 100-file result | Stable bounded top-k plus independent total count | At most 100 retained hits |

### `crates/tools/file/src/notebook_edit.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F03: 191 | MEDIUM | Data structures | Each notebook cell subprojection initialized whole-arena indexes | Borrow validation/ID lookup; import unchanged cells once; materialize changed cell | Removes cells × arena-size setup |

### `crates/telemetry/src/bus.rs`

| ID / baseline lines | Severity | Domain | Pattern | Fix | Impact |
|---|---|---|---|---|---|
| F26: 63 | LOW | Logging | Disabled telemetry still built event metadata | Lazy builder after unchanged privacy/killswitch gates | Zero event-metadata construction when disabled |

The rooted-lock change also applies to `crates/platforms/posix-minimal/src/fs.rs` and `crates/platforms/windows/src/fs.rs`. Shared top-k, cancellation, and bounded-reader code lives in `crates/tools/file/src/shared.rs`. Lazy telemetry is used by Read/Edit/Write/NotebookEdit and path-blocked events.

## Measured and mechanical evidence

| Fixture | Before | After | Verification |
|---|---|---|---|
| 256 metadata lookups, 4 MiB cached content | 1,073,741,824 allocated bytes / 256 allocations | 0 bytes / 0 allocations | Counting allocator; equal scalar values |
| 2,048 filename validation items | 2,048 owned item projections; 8,396,800 arena-index slots initialized | 0 item projections/index slots | Same validation result and diagnostic differential tests |
| 2,048 notebook cells, one replacement | 2,048 cell subprojections | One changed cell materialized; 2,047 unchanged cells imported | Serialized mutation/result bytes equal |
| 2,048 notebook text cells | 211,010,409 UTF-16 prefix units recopied | 0 prefix units recopied | Final content-block bytes equal |
| 50,000 search candidates | Full hit list plus sort | 100 retained hits; heap capacity 128 | Stable full-sort reference equality |
| 10,000 formatted search rows, page of 17 | Full formatted list | 17 retained rows | Full totals and pagination equal |
| 15,000 only-matching spans / 30,000 input bytes | Repeated scans from byte zero | 29,998 cumulative prefix bytes scanned | Matching rows and line numbers equal |
| 32 tracked files, 2 / 100 / 1,000 history snapshots | Entire snapshot history cloned per query | 32 selected backup clones in each case | Latest target, earliest v1 fallback and source authority retained |

Raw timing observations are in `outputs/performance-probes.json`. They are single local comparative runs under concurrent compiler load, not an end-to-end throughput or production-latency claim. Correctness assertions use byte/count comparisons rather than timing thresholds.

## Validation

- Native Claude Code 2.1.322: **403 cases and 64 pinned ranges passed**, including 18 bundled-ripgrep searches.
- Default file tools: **443 passed**, 1 manual probe ignored by the ordinary suite.
- All file-tool features: **486 passed**, 2 helper/manual tests ignored by the ordinary suite. Child-process helpers are explicitly invoked by their owning tests.
- Three ignored scaling probes were run explicitly and passed; the standalone allocation probe passed at 1 KiB, 256 KiB and 4 MiB.
- Supporting permission/session/platform/rooted-fs/tool-api/telemetry suites: **3534 passed, 3 failed, 4 ignored**. All three failures reproduce on the clean baseline:
  - `mobile_linux_sandbox_uses_selected_profile_for_project_source_writes_and_cwd`: existing `.atlas-workspaces/apps` versus `.atlas/.apps` expectation mismatch.
  - `connect_http_fetch_timeout_drops_slow_post`: existing timeout returns a routed error while the test expects a pending call.
  - `kill_tree_terminates_grandchildren`: host rejects process-group SIGKILL with `EPERM`.
- Final primary-workspace integration build: blocked by a concurrent change in untouched `crates/llm-runtime/src/service.rs:6818`: the pinned SDK `AnthropicRequestPolicy` has no `plain_text_message_content_indices` field (E0560). This field is added in the primary workspace diff and is absent from the optimization patch. The landed filesystem files match the tested isolated source; full primary-workspace acceptance remains blocked by that SDK mismatch.
- Rustfmt and scoped diff checks passed. Two independent implementation reviews found no remaining actionable regression after the PDF/FIFO corrections.

Commands and target-by-target results are recorded in `outputs/verification.json`; baseline reproduction logs remain under `work/`. The first all-feature build ran out of disk space; its log is retained and the completed retry is the reported result.

## Improvement plan and status

1. **Completed — runtime progress and cancellation:** bounded blocking Read/media work, chunk/match stop checks, pinned nonblocking lock attempts, FIFO-safe opens.
2. **Completed — PDF authority and cleanup:** render the captured snapshot; retain child/scratch/permit ownership through termination and off-runtime cleanup; preserve legacy process-failure and cleanup-error results.
3. **Completed — allocation and projection costs:** metadata-only read state, in-place aliases, borrowed validation, subtree materialization, linear text accumulation, early image-buffer release and deferred notebook rendering.
4. **Completed — search scaling:** stable bounded top-k/page retention with complete totals, linear only-match prefix accounting, coherent policy preparation.
5. **Completed — permission preparation:** bounded cache replacement, stable hashed dedup, one case fold and one marker-occupancy scan.
6. **Completed — bounded filesystem/history work and logging:** delegated guest prefixes, metadata-bounded tails, per-file restore plans, reused stale-edit analysis and lazy event metadata.
7. **Deferred — mutation disk-I/O offload:** Edit/Write/NotebookEdit still perform synchronous mutation I/O. `block_in_place` is invalid inside a Tokio LocalSet even with a multi-thread runtime; detaching spawned writes after caller cancellation would weaken the existing ownership contract. This needs an owned mutation transaction design rather than a mechanical offload.

## Coverage of audit domains

| Domain | Result |
|---|---|
| Database/queries | No SQL/ORM layer in scope; duplicate prefix-read observations counted under I/O |
| Memory/resources | Implemented content, alias, image, page and history-copy reductions |
| Algorithmic complexity | Implemented top-k, pagination, linear prefix/text work and stable hashed dedup |
| Concurrency/async | Implemented bounded Read/media offload, lock progress and process ownership |
| Bundle/dependencies | No proven shipped-size optimization; existing pins and codec support retained |
| Dead code/redundancy | No performance-relevant deletion; compatibility/transcript APIs retained |
| I/O/network | Implemented bounded prefixes/tails; no fresh-read dedup introduced |
| Rendering/UI | Implemented notebook work admission; no frontend tree in this subsystem |
| Data structures | Implemented borrowed/subtree projection and buffer changes |
| Error/resilience | Implemented deadline/cancel/FIFO/process cleanup corrections |
| Caching/memoization | Implemented compilation-cache replacement and coherent policy preparation |
| Build/compilation | Rejected speculative codec/Rayon feature removal without shipped evidence; release profiles unchanged |
| Security-performance | Preserved path/descriptor/policy validation; corrected PDF snapshot and FIFO races |
| Logging/observability | Implemented lazy metadata after the existing privacy/killswitch gates |

Only a direct dependency on the workspace-existing `tokio-util` was added to tool-file; no external dependency version changed.

## Remaining limits

- Individual OS syscalls, regex operations, and codec stages remain uninterruptible. Checks run between chunks/stages. Read/media concurrency is four; render-process concurrency is two. Offloading adds a thread-handoff cost for small reads.
- Multiline grep still buffers a file. Its existing per-file 10,000-record valve remains. Explicit unlimited pages and large filename offsets intentionally retain their requested storage semantics.
- The 256-entry compilation caches can thrash with larger active working sets, and concurrent cold misses may compile twice. Permission decisions and filesystem authority are not cached.
- Contended rooted locks retry every 10 ms without a deadline or reopening the capability. This trades small scheduling overhead for runtime progress.
- Renderer stderr remains inherited, preserving prior returned process-error bytes; Poppler diagnostics may name the private staged input. Cleanup failures are logged and retain the prior result. Detached cleanup requires a live Tokio runtime.
- Windows code was updated/reviewed but not runtime-validated on this macOS host. No packaged Desktop, physical-device, production, or soak acceptance is claimed.
