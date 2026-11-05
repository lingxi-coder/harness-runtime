# Windows process source validation, 2026-11-04

The [Windows diagnostic run](https://github.com/lingxi-coder/harness-runtime/actions/runs/41863917085/job/126693012876) succeeded on `windows-2025` with Rust 1.94.0. Its checkout was `3f1fb337f535ccf813c1eb8cac17b968d4f95750`; the actual job log was inspected for the results below. The run used a temporary diagnostic workflow and scripts on that branch.

The tested Windows source files match these source-only commits byte for byte:

- `2f0a82a3211d89d06e345a80b9b16e6400ac232d`: exercise armed and disarmed job closure with a live child in `supervisor_gate.rs`.
- `a5b31079b528a65b7fab33bded19f88835ba9512`: create the private supervisor directory with the process `TOKEN_USER` as owner while retaining strict owner and DACL validation in `supervisor_native.rs`.

The tested `rooted-fs` file also matches `65877dbd1c518cce2c92297e3407da92c46a19ee`, already on main. No second copy of that change is needed.

The diagnostic temporarily reverted the append-handle attribute access and explicit directory owner, ran baseline checks, and restored the candidate files before the passing checks:

- The baseline append-handle test failed with `PermissionDenied("task.output")`; after restoration it passed, as did the pinned-root replacement rejection test.
- Under PowerShell, the baseline private-directory test failed with `supervisor directory owner differs from current user`. The owner probe showed that the runner's default owner was Administrators, whereas explicitly setting `TOKEN_USER` produced the expected owner. A Git Bash probe also exercised a forced Administrators default owner.
- The baseline foreground-stop test failed its cancellation assertion. This combined baseline check does not isolate which reverted change caused that failure.
- After restoration, private-directory creation and validation passed. The negative test rejected a wide DACL and a preexisting directory; its foreign-owner control actually ran and rejected the directory, rather than taking the permission-denied fixture fallback.
- The complete `platform-windows` library suite passed **39/39** tests. The armed/disarmed job closure test and `native_job_stop_reports_waited_status_before_exit` each passed three further iterations.

These are hosted Windows source-test results for the recorded checkout, including controlled baseline mutations. They do not establish a packaged-product or end-to-end acceptance result for another revision.
