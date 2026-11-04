# Error snapshot allocation ownership

`git_error_save` makes an owned copy of a dynamic error and its message. If the
message allocation fails, it must release the already allocated error object.
Both allocation failure paths continue to return `-1` and the static OOM error.
No-error and other static errors remain allocation-free.

A caller that restores a successfully saved dynamic error recovers the original
diagnostic after cleanup. If saving fails, the supported fallback is the static
OOM or thread-local initialization error, not the original diagnostic.

`python3 scripts/tests/test_git_error_save.py` extracts the production snapshot,
restore, free and string-duplication functions into a small C harness. Its
allocator and thread state are simulated; it covers both allocation failures,
normal deep-copy and restore, static errors, no error and missing thread state.
Set `GIT_ERROR_SAVE_ASAN=1` to compile this harness with AddressSanitizer.
