# Independent finalizer review

Static review only; no assembler, validator, Cargo, or Native command was run. Reviewed script snapshots:

- `finalize-repair-receipt.py` SHA-256 `d86ffd036f6d15672117a4d2ada1f60098feff89f740ae438ece4474cf7c8835`
- `assemble-ledger.py` initial SHA-256 `281aa95d6e4c511ff90fa952492b0af1ed00716cad3228c40bf8ccec8c5076c7`
- `validate-ledger.py` initial SHA-256 `610990ce2dd67dee2aa5b18d8cd2234aee2a83a9ce5aaf375bc6475ed733f37a`

Current hashes after the finalizer/assembly follow-ups:

- `finalize-repair-receipt.py` `5e4163a7f7bdf73b22d478cc21c124c43f5f9000f8958d4a5595ff77b36e8fd2`
- `assemble-ledger.py` `c9c8dd7eaea1207653905e0b043572ca78c89da24054538e029ec3de8f622c84`
- `validate-ledger.py` `44d91fd60db24d603c0122e254b2205f46c2e26295bfcb0461621670865aec7a`

## Re-review of the validator update

The current `validate-ledger.py` SHA-256 is `44d91fd60db24d603c0122e254b2205f46c2e26295bfcb0461621670865aec7a`. The row-to-audit gap is closed: `render_expected_contract_rows` reconstructs every row from the frozen baseline, ownership map and SDK/runtime/Mods audits, including the exact scoped gates/root rows; `validate_rendered_contract_rows` compares each complete row dictionary. This checks `owner`, `audit`, `assessment`, nested `status`/`byte_status`, all source fields, and rejects unexpected row fields. A changed row assessment therefore fails before the ledger can validate.

The follow-up also validates the exact ledger schema, `contract_count == len(baseline['contracts'])`, and the existing assembler status literal. This is a direct equality check, not a status-word whitelist, so it preserves the current bounded vocabulary and rejects a changed top-level byte-parity claim. The renderer mirrors the assembler's existing output rather than requiring Native-byte evidence for a row whose declared evidence class is source or regression.

The owner reports that the original 42-row rendering passes and in-memory negative cases reject a changed assessment status and changed top-level status/count/schema. I inspected the code and fixed literals read-only; I did not execute these checks myself.

Archive-only follow-up: `assemble-ledger.py` is now SHA-256 `4f6ecf24e20e612a4436e716a5e11637eed17c4b8a76d7ad979c4856c9797451`; the only reported change adds the two independent reviews, cancellation review Markdown/JSON, and macOS aggregate script/runbook to the existing evidence copy list. It does not change row rendering or validation. Root reports the 140-file closure check still passes; I did not rerun it.

## Finalizer delta review

The finalizer follow-up preserves the evidence guards while adapting to the actual receipt formats:

- H phase `tested_source_patch_sha256` now reads the selected snapshot's nested `sha256`, which covers r9/r10 as well as r6/r8 without changing the source snapshot binding.
- The Product command parser recognizes only the runner's unlabelled `cargo metadata ...` record as the `metadata` phase and retains the full command text. The exact eight-phase name set, required command fragments, and progress log with one start/exit per phase and exit code zero remain required.
- Empty stdout is accepted only for `product-fmt`. The phase still requires a captured `fmt.log`, uses the nonempty progress log as its primary receipt after `parse_progress` validates every Product exit, and archives the empty stdout plus its SHA-256 separately. Other Product logs still must be nonempty.

The saved `repairs-final-17593.json` records `status: passed` and 20 phases; its Product phases are bound to P17593/H44. Root reports the Product runner's eight phases and finalizer completed. I did not execute the finalizer or any gate.

The first assembly after this change would have omitted the empty Product fmt `stdout_log`: the validator requires both `phase.log` and `stdout_log`, while the previous assembler copied only `phase.log` for Product phases. Root has closed that path in the current assembler by copying the entire `final-evidence-17593` tree, the `mac-package-evidence-17593` tree, and the original named repair receipt in addition to its alias. This retains the empty fmt file and the logs referenced by the final receipt. The final assembly/validator run must exercise these required paths; I did not run it.

## Confirmed boundaries

- Failure retention is explicit for the known superseded Harness failures: r6 `runtime-lib` and r7 `hooks-lib` must match their recorded failure codes and diagnostic text, and their logs and exit receipts are recorded with SHA-256. The assembler copies all five Harness gate directories. An incomplete/failed finalization does not publish a passed receipt and does not remove the original inputs.
- Reuse is source-snapshot-bound: r6 selected phases, r8 Hooks/docs, and r10 runtime/Clippy/fmt have explicit origins and patch hashes; the r6→r7 test-only proof and r8→r9→r10 single-file proof are checked against committed H. The r9/r10 execution HEAD of `b3864c6c` is consistent with their `candidate-head.txt` and dirty source patch; r6–r8 correctly fall back to baseline HEAD because they have no candidate-head file.
- The frozen baseline contains 42 unique contract IDs, including `MCP-1`; the historical count of 43 included a separate MCP domain heading. Owner counts are SDK 14, runtime 18, Mods 7, gates 2, root 1. Assembly preserves the owner audit row contents and marks target freeze separately. The updated validator now checks that each assembled row remains exactly that rendering.
- Product 17593's observed platform runner log now ends with `EXIT product 0`; `repairs-final-17593.json` is present and passed. The finalizer requires completed zero exits, matching final pins, a complete Product runner, package receipt, and CLI run-04 receipt before publishing its success outputs. This review did not run the full ledger validator itself.

## Review disposition

The finalizer's current gate and source binding design is adequate for preserving the known failure evidence and distinguishing tested snapshots. The 42-row assembly does not synthesize raw-byte or universal whole-gate passes, and the validator binds every row and its top-level scope wording to the assembler's fixed rendering. The silent-fmt evidence omission found in the intermediate assembler was closed in the current copy plan. No remaining source-format issue was found in this read-only review; final assembly and validator execution are not independently confirmed here.

## Final provenance and assembly delta

Read-only review of the final script snapshots:

- `finalize-repair-receipt.py` SHA-256: `a3b54a27e530d73a768c7a1c43750e848ae1af5ef0f97bfe8e90ca41eb7fb05c`
- `assemble-ledger.py` SHA-256: `179d295db3d5e032c26e09a7998fb31cc3e2c469583581cd8eca49697f004ae8`
- `validate-ledger.py` SHA-256: `4be81336a2629ff661c6fdb58141963a8517637f476a192f1d0578df4f3c5325`

The r9/r10 binding now resolves through fields the finalizer actually emits: `r10_candidate_head` must equal both `source_patch_snapshots.r9.execution_head` and `.r10.execution_head`. This replaces the validator's former nonexistent `r9_candidate_head` reference and preserves the shared tested execution-head constraint. The finalizer also keeps `current_control_blob` and the run.rs `current_blob` distinct; `current_control_git_blob` and `r10_final_run_git_blob` are emitted from their respective source files. The v2 final baseline points to `repairs-final-17593-v2.json` by its recorded SHA-256, and the validator checks the run.rs blob against the r10 source delta.

The assembler copies the canonical preservation JSON files and `matrix-introduction.md`, then renders `current-matrix.md` from the assembled 42 contract rows. The prior matrix is preserved at `harness-runtime/docs/parity/current-alignment/archive/current-matrix-before-r376-20261018.md`. This retains the prior document while making the generated current matrix derive from the same row set as the JSON ledger.

Root reports that the final assembly and validator ran successfully with all 42 rows accepted, after the 20 repair phases, eight Product runner phases, and package checks passed. I inspected the final code and provenance references but did not rerun assembly, validation, Cargo, or Native. This status supersedes the earlier note that final assembly/validator execution had not been confirmed by Root.

## Root post-review archival note

After the independent delta review, Root added only the canonical-preservation archive entries, generated the Markdown table from the same 42 JSON rows, and added evidence-local Git attributes to preserve raw bytes and exclude immutable log/patch whitespace from formatting checks. Final assembly, the 42-row validator, and staged diff checks passed. The final assembler SHA-256 is `b9fe157ee32b12cdc955724cc65bb21be64ec186e61f9ffb136f74df2c9f6454`. This note is Root execution evidence, not an additional independent review.
