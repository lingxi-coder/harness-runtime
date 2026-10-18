# Claude Code 2.1.295 — current acceptance ledger

The reference is frozen at **2.1.295**, binary SHA-256
`0116ee2e0a513900b633d9951367f18747686478e2b462805b8c31609f047f70`.
No later version is in scope. Source agreement, executed extracted-source
comparison, production regression, raw-byte comparison and lifecycle
constraints remain distinct evidence classes.

## Candidate status

**Current candidate: H `44f2eef5` is delivered and pushed; Product
`17593eaa66dc762d5c9c581ba348398c914bdbcd` is verified and pushed to main. Its full tests,
five platform checks, public CLI SIGINT gate, fresh signed macOS package, and
packaged GUI acceptance have passed. The 42-row matrix assembly and validator passed.**
SDK `a363c5b3` is unchanged. The immutable
`baseline.json` preserves the starting combination (SDK `a363c5b3`, H
`4d6d0d6c`, Product `f71087b4`) and all **42 original unique contract IDs**.
The historical count of 43 incorrectly included the MCP domain heading. Earlier
receipts remain evidence only for their pinned inputs; they do not certify the
current candidate.

H `44f2eef5baf8ae16254934dbca451cbfd528d955` is pushed to `main`; r10
formatting, runtime (1,468 passed, 0 failed), and Clippy gates passed. These
affected-source gates do not claim a blanket full-workspace H pass. H includes a
narrowly guarded streaming-cancellation result projection. Native projection
parity remains unverified when session compaction retargets a preserved message
ID or when interruption occurs before the provider stream is active.

Product `17593eaa66dc762d5c9c581ba348398c914bdbcd` includes the current pin,
public CLI SIGINT regression, four self-contained fixtures, and committed ledger
docs. The full runner at
`/private/tmp/product-full-r342-r376-20261018T084434Z-98045/runner/run.json`
completed 2,740 passed, 0 failed, and 4 ignored across 40 test targets and 9
doctest targets. The five-check final-pin platform runner passed at
`product-platform-final-run-17593.log`. Public CLI run04 passed 15/15 checks in
4.071 seconds with no forced cleanup and no Native launch; its receipt is
`cli-sigint-candidate-run-04-17593eaa66-20261018T091547Z-1496/receipt.json`.
The fresh P17593/H44f2 macOS Flare package passed static audit, startup smoke,
independent audit, and actual packaged GUI input/Submit acceptance. Its
development-signed ZIP SHA-256 is
`64c8a4d7a441f53c5dc9990d13f3ea4930f1ead72c2df52759565102f73481de`; it is not
notarized. The actual GUI typed `packaged-host-cua-value`, appended `xy`, and
submitted successfully. See `receipts/mac-package-final-17593.json` and
`mac-package-evidence-17593/`. Finalization passed the 20 repair phases and
eight-phase Product runner recorded in `repairs-final-17593-v2.json`; current-pin
platform, baseline, and gate receipts are `platform-final-17593-v2.json`,
`final-baseline-17593-v2.json`, and `gates-audit-final-17593-v2.json`. Final matrix
assembly remains pending. Product 8610 is historical: its 2,739 tests passed
with zero failures, but its run04 public CLI SIGINT candidate emitted the wrong
terminal result. That outcome does not certify or contradict the new Product
pin. The public cold-resume selector remains Product regression evidence, not a
Native cold-resume oracle.

The 2.1.295 Native alias/glob semantics remain **unverified**. The bounded
endpoint-only probe (`native-ltr-alias-probe-approved-endpoint-run/receipt.json`)
is `capture_invalid`: the positive Edit-deny
control was not observed, so its catalog observations cannot establish alias or
glob behavior. The Native `LTr` helper body/regex flags have not been bound to
the fixed artifact. This is neither an accepted difference nor a parity pass.
H source and regression coverage are separate evidence; they do not substitute
for the missing Native semantic evidence. Any contract requiring exact Native
alias/glob behavior remains unverified until valid evidence exists.

The final ledger must use these current pins and their own receipts, and retain
the distinction between source agreement, focused regression, executed
extracted-source comparison, raw-byte comparison, and lifecycle constraints.
Do not infer a universal parity pass from green source gates or focused tests.

## Reuse and limits

Historical results apply only to unchanged inputs/source or an explicitly
covered delta. The old full Harness workspace gate is 20,471 passed, 0 failed,
7 ignored at `dc6ce121`; later source changes require their own affected gates.
A full Product workspace run does not execute dependency-crate tests. The old
r375 signed macOS package validates its original pins, not new repairs.

The HTTP seam compares headers except the dynamic Host and substitutes only
the documented 64-byte device-ID span in the body. It is not unrestricted raw
body equality. CLI error checks validate fixed fields and source units, not
whole UUID/time-bearing stdout. Existing 2.1.293 matcher fixtures retain their
historical label. Extracted 2.1.295 source execution is not a public Native
capture. Mobile cross-compilation does not establish physical device behavior.

## Scope

Branding, multiple providers, Fusion and LingXi UI remain intentional
differences. All LLM communication belongs to llm-client. Current APIs only;
no obsolete-version compatibility. Remote and cross-session discovery belong
to the separately owned project protocol. Local Apps parity and replacement of
Electron's runtime are excluded; actual Local App tool defects remain in scope.
Computer Use implementation remains with the other session. Bun is 1.4.3.
Full Native TUI layout/ANSI/key replication is excluded. `/heapdump` is an
accepted Rust/Bun runtime difference without a fake implementation. Claude Test
account/service gates are accepted differences; local testing is a separate
plugin. These differences are not recorded as byte parity.

## Contract dispositions

Each row retains the original requirement and links to its owning audit.
Pending evidence cannot be upgraded to a pass by relabeling it.

| ID | Disposition | Audit |
| --- | --- | --- |
