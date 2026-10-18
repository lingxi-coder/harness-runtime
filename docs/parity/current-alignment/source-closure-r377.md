# r377 source closure — Claude Code 2.1.295

Native stays pinned at 2.1.295, binary SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. The
r377 H candidate is commit `6548b9d5e702f8e9d42772412cd65b72881e24b4`,
based on H `e4e00ec16acfc8702d03799258095676cc203bf0`; the prior production
source identity is `44f2eef5baf8ae16254934dbca451cbfd528d955`. Product is now
`a2a45fd0e845558ce55aa35e6bd9f57ed4491ed3` and pins this H production commit; SDK remains
`a363c5b3a237d53eea12c3abfb3b98648ef15d3e`. The bounded H gates passed:
selected-r9 reports 120 passed, 0 failed, 0 ignored across 18 selected rows;
format-r4 and full-workspace, all-targets Clippy-r4 (desktop, client adapter,
and UniFFI features) both exited 0. The compact receipts are in
[`h-gates-r377.json`](evidence/source-closure-r377/h-gates-r377.json).
The scoped Product gates also passed; see [stage receipt](evidence/source-closure-r377/stage-receipt.json). These results do not claim full-workspace tests, all 42 contract rows, or public Native byte parity.

R376 receipts remain historical evidence for their recorded source pins. This
follow-up Native process count is 0; it created no fresh GUI package or GUI
session and did not re-run the old invalid public alias capture. Extracted-source execution,
candidate regression tests, and public Native output are separate evidence
classes.

<a id="alias-glob"></a>

## Alias and tool-name glob — TOOL-1 / MCP-1

The fixed artifact directly binds `LTr(e){return h(e)}` in
`src_186325155.js` (offset 186325155, 7,419 bytes, SHA-256
`a66f6f6d6f121ab2a8a9b5acf8ad8fa5d95ce3c6b55175dd7bbf5463b3567f0b`). `h`
splits on literal `*`, requires the literal prefix and suffix to match the
whole tool name, and finds nonempty interior segments in order through helper
`P`. It uses JavaScript string-index and `charCodeAt` behavior, not `RegExp`.
The optional `initialize.toolAliases` map is single-hop; the permission context
stores it and the deny matcher checks canonical names and applicable immediate
alias sources. Source-specific proxy expansion and glob checks are documented in the tracked
[pinned Native source bindings](evidence/source-closure-r377/native-source-bindings.md)
and the H consumer audit retained in the local r377 acceptance records.

The source-only Node probe executed the extracted `P`/`h`/`LTr` functions on
nine deterministic cases, including LF/CR/U+2028/U+2029, a terminal LF, and a
wildcard between surrogate halves; all matched the expected results. H's
matcher and live consumer chain were source-reviewed, and the selected-r9 H
selectors passed (receipt linked above). Source execution and H regressions
are not a Native launch or provider/catalog capture. The older public alias probe remains
`capture_invalid` because its positive Edit-deny control was not observed; it
does not establish runtime alias semantics. Accordingly the source question is
closed, while public Native catalog/byte parity remains unobserved.

<a id="cancellation"></a>

## Cancellation result projection — TURN-3

Pinned source binds the ordinary one-shot stream-json print path. The producer
counts yielded `user` rows in `Dt`, selects the final row that is not a turn
companion, and for eligible `aborted_streaming` / `aborted_tools` markers emits
`error_during_execution`, `is_error: true`, and the `[ede_diagnostic]` result
unless `ZVo` suppresses it. Suppression includes the documented final
assistant text/thinking/redacted-thinking case, a user row made only of tool
results, or `stop_reason === "end_turn"`. `YGr`/`nqo` supply the diagnostic
and current-query error frames. The query source yields distinct streaming and
tool-use interruption markers. Exact source ranges and hashes are in the
tracked [pinned Native source bindings](evidence/source-closure-r377/native-source-bindings.md);
the local r377 acceptance path retains the detailed cancellation projection report.

The candidate carries query-local accepted user-row counts and a typed
interruption marker through the publisher rather than recovering them from
message IDs or transcript text. This keeps those facts stable when Mod
compaction rebuilds prompt/event/marker IDs. The matching H regression
selectors passed in selected-r9, including the ID-rebuild fixture; these are
publisher-level tests, not a live compaction/provider run. The prior run04
receipt remains the narrow public capture: accepted POST, withheld provider response, SIGINT, and
its selected terminal fields. It is not whole-stdout equality. Pre-admission
cancellation, shutdown/`QueueNow`, multi-turn stream-json, and engine-deferred
slash paths remain outside this source closure; no Native result for those
cases is asserted.

<a id="frozen-deny-order"></a>

## Frozen command denies and order — CMD-1 / PROMPT-1 / SESSION-4

Native `YEn` accepts an optional `frozenCommandDenies: string[]` of at most
1000 entries and 1024 JavaScript UTF-16 units per entry. `Se` copies the
current `alwaysDenyRules.command ?? []` into the fork context without
capture-time sorting or deduplication. The recovered `fLn` consumer merges
`frozen → live Command → skill additional` and applies `[...new Set(...)]`:
exact string equality, first occurrence retained, with no trim, parse, or
canonicalization. The fixed-source binding and its bounded module inference
are recorded in tracked [pinned Native source bindings](evidence/source-closure-r377/native-source-bindings.md);
the detailed frozen-deny consumer report remains in the local r377 acceptance path.

The r377 candidate now carries the frozen raw projection separately from
ordinary permission layers. Its effective single `PermissionRuleSource::Command`
sequence is `frozen → live Command → current command-input → per-call
additional`, with exact UTF-16 projection deduplication before rule parsing.
Live rules use `original_rule_projection()` where available and otherwise the
canonical projection. Keeping one source is necessary because same-source
`Read(!...)` negations cancel or re-establish preceding matches. The old
`frozen_command_deny_layers` path was removed. The three-stage regression uses
frozen `Read(src/**)`, current-input `Read(!src/public/**)`, and per-call
`Read(src/public/keys/**)`: the public index should be allowed and the keys
path denied. Additional focused selectors distinguish `Bash` from `Bash(*)`,
preserve `[D800, FFFD, D800]` through the real parked request `write_row` /
`read_row`, and check raw dispatch context forwarding. These H selectors passed
in selected-r9. The JSON string-array/UTF-16 schema, extracted-source findings,
and candidate transport regressions do not constitute a public Native
permission outcome capture.

<a id="cold-resume-utf16"></a>

## Cold resume and UTF-16 suggestion — CMD-1 / SESSION-3 / PROMPT-2 / SESSION-5

The fixed Native cold replay path for object-valued `tool_result.content`
stringifies it, applies a 50,000-UTF-16-unit limit at a scalar-safe boundary,
counts omitted Unicode scalars, and emits the Native suffix. The H source review
traces producer, JSONL persistence, fresh-process parsing, history construction,
request preparation, and SDK encoding without finding a concrete mismatch; see
the [pinned Native source bindings](evidence/source-closure-r377/native-source-bindings.md).
The detailed producer-to-SDK review remains in the local r377 acceptance path.
The actual Product cold-resume selector passed again on the pinned r377 candidate, verifying Skill U+D800 persistence and U+FFFD provider projection. There is no matching public
Native cold-resume trace, so neither the source review nor historical Product
selector is a Native byte oracle.

<a id="utf16-suggestion"></a>

For prompt suggestions, Native 2.1.295's `W` normalizer trims/wrapper-filters
candidate JS strings and `q` returns the first nonempty value without scalar
conversion. Candidate changes retain UTF-16 source units through side-query,
prompt-result, and outgoing JSON paths while using display projections only
where needed. The selected H suggestion and UTF-16 helper regressions passed
in selected-r9. Product Bridge and CLI gates passed on the current pins; mobile source/helper checks passed within their recorded limits. The
public Native U+D800 `prompt.suggest` frame and generated-name scalar values
are unobserved; the extracted-source finding and H helper tests do not
substitute for them.

## Validation boundary

H format-r4, selected-r9, and Clippy-r4 results are recorded in the linked
receipt. Product workspace all-features/all-targets check and Clippy, formatting, 99 Bridge tests, Bridge/Electron TypeScript checks, canonical source validation, docs sync/check, and three actual CLI integration tests passed. Swift helper tests (6), Kotlin helper assertions (6), Swift syntax (13 files), and generated Client Swift binding typecheck also passed. The binding check combines current Client output with saved wrapper bindings; it is not full app typechecking, Rust linking, or device execution. No full 42-row
rebuild or full-workspace test pass is claimed. Native process count is 0; no
fresh GUI package or device launch occurred. No public alias catalog,
cancellation stdout comparison, cold-resume byte capture, or suggestion-frame
capture is claimed. Source-bound behavior, extracted-source/helper execution,
H candidate regression tests, and public Native bytes remain separate evidence
classes.

Product delivery includes documentation-only commit `7ea23b6ae96d88ab1919cf7f00c44115f2fb562c` above the tested Product source commit; runtime and dependency sources are identical.
