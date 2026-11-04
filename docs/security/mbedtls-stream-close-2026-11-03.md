# Mbed TLS stream ownership repair, 2026-11-03

The wrapper now transfers input-stream ownership only after successful construction. All local wrapper/context/hostname allocations are released on setup and allocation errors; the caller retains the input on failure. This resolves the separate wrap failure cleanup issue and was committed first as fb6cbef9.

The close follow-up always closes an owned input even when TLS shutdown fails, clears connected state, and preserves the earlier TLS diagnostic with the existing libgit2 error save/restore API. Borrowed inputs remain owned by their caller. The close return value is the TLS failure when both operations fail, otherwise the input close result. As documented by libgit2, failure to allocate a saved diagnostic can substitute its out-of-memory diagnostic; this repair does not override that API contract.

An independent Codex AI-assisted review read actual callers, allocator and error contracts. It found the initial diagnostic-overwrite gap, then re-reviewed the corrected snapshot. Exact production bodies are executed by a C99 harness with deterministic allocation/setup/teardown/close callbacks: all three wrapper allocation failures, setup failure, new/borrowed success, socket creation failure, connected TLS teardown failure with borrowed/owned inputs, and combined error priorities. Normal and AddressSanitizer tests passed. The original source, skip-owned-close mutation and skip-diagnostic-restore mutation were rejected by their intended assertions. These are controlled local tests; TLS and OS behavior are substituted, no CA lifecycle or device acceptance is established.

Backend SHA-256: 5327d3802d061de2f47b7596556e1f6b3b63b3aabc38c01876bd974dac1a22ba

Regression test SHA-256: 7e695f220849a76462f9ad98591b9e87fa99717bb2ea17e211bbab11199448bf

The independent shared CA configuration race remains unresolved. The Mbed TLS 3.6.7 native source audit is incomplete. This is a development checkpoint, not a main delivery or complete TLS safety approval.
