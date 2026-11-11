# Filesystem review fixes — 2026-11-10

This follow-up addresses all eight findings from the full filesystem review of
runtime `6e743f5ae7a437d0ae68bd90311d2d16491351ca`. The SDK remains pinned to
`54cf6b7b96e36c16593e43e170d80767099fbe7d`.

| Finding | Resulting behavior |
| --- | --- |
| FS-01 | Windows opens the approved root component by component from a drive, share, or volume anchor. Every filesystem component uses a handle-relative no-reparse open. Root acquisition no longer canonicalizes through a replaceable junction. |
| FS-02 | Read checks metadata from the opened descriptor and consumes at most the admitted bytes plus a growth sentinel. Oversized files retain only a diagnostic prefix. Write rejects missing/partial prior Read and clipping fences before loading the old body; eligible snapshots are length-bounded and cancellable. Private memory writes retain their admitted identities. |
| FS-03 | Ordinary main-loop notebook Read results preserve their image outputs in the provider request. |
| FS-04 | A matching, provider-visible inline-PDF attachment is the single document owner in main and Agent execution. Missing attachments retain structured fallback; extracted page images remain available. |
| FS-05 | Edit and Write prepare exact UTF-16 diffs and encoded output on bounded blocking workers before committing. Cancellation prevents a subsequent commit. |
| FS-06 | Present search paths with invalid UTF-16 fail validation rather than selecting the current directory. |
| FS-07 | Search authorizes the resolved root or nearest existing ancestor before existence/type diagnostics and sibling suggestions. Authorized missing-path messages remain available. |
| FS-08 | Glob obtains rooted metadata without requiring content-read permission. Unreadable matching files no longer abort the entire listing, while candidate replacement checks remain enforced. |

## Resource and compatibility details

Ordinary text retains its existing context-derived byte allowance. Explicit line
ranges retain bounded streaming, including the alternate screenshot spelling
path when image support is disabled. Notebook and PDF snapshots use their
existing physical size limits. Encoded image inputs now have a separate 100 MiB
source limit before decoding. Oversized inline PDFs report their size error
before page-count parsing or model-support checks.

An already-running synchronous diff can finish after its caller cancels. It
retains its preparation permit until it finishes, and cannot proceed to commit.
This preserves exact successful patch output without blocking the async worker.

Windows accepts ordinary and extended drive/UNC paths and volume-GUID roots;
raw device and `GLOBALROOT` namespaces fail closed. The implementation follows
the documented [lexical absolute-path behavior](https://doc.rust-lang.org/std/path/fn.absolute.html)
and [Windows open flags](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew).

## Validation evidence

The reproducible commands, source hashes, regression coverage, fresh CLI wire
captures, known baseline failures, and preservation receipts are recorded in
`artifacts/filesystem-review-fixes-20261110` in the enclosing LingXi workspace.
`fix-manifest.json` maps each finding to implementation and regression coverage;
`validation.json` contains the final aggregate result.

The CLI validation uses an isolated app checkout with task-local Cargo path
overrides to the fixed runtime. It does not change published dependency pins.
Windows code and tests are cross-compiled with the installed stable toolchain;
the native matrix uses repository-pinned Rust 1.94. Windows runtime execution
requires a Windows runner and is not represented as executed evidence.
