# Frozen 2.1.295 refusal source and bounded oracle

Source excerpts bind to Native SHA-256 `0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`. They prove source semantics, not public raw-byte equivalence.

The disabled-retry tool-boundary run used an isolated HOME/cwd, a fake key and a localhost-only sandbox. Run 001 failed sandbox parsing before Native started (exit 65, zero requests). Run 002 observed running and queued tools complete successfully, a transcript-only API error before results pending at refusal, and a follow-up request containing tool results without the API error or safety nudge. It was stopped at the 18-second process-group bound after the stub rejected later requests; generic follow-up termination was not verified. Already-completed tool results may precede that error.

No additional Native launch is required or authorized by these archived files. Candidate gates are recorded separately; this oracle alone does not certify the integrated candidate.
