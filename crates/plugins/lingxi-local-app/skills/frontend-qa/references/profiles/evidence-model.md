# Evidence-first workflow

- Start with browser verification for the preview URL: primary path, console,
  network-visible failures, and screenshots.
- Then verify the same flow through native WebView inspection and logs because
  bridge behavior, device context, and system back are host-specific.
- If Browser is unavailable, mark verification as degraded, continue with the
  native inspect/act/log path, and do not claim full visual Browser coverage.
- Record evidence before suggesting fixes. A claim without a screenshot, log
  line, console message, UI snapshot, or deterministic reproduction step is not
  a finding yet.
- Prefer one concrete finding with good evidence over many speculative issues.

Host QA evidence is scoped. Use only evidence recorded for the returned
`qa_handle`, its in-scope target/scenario, and the exact `qa_evidence_ids`
returned by Host tools. Preserve the Host upstream finding ledger in every
post-repair candidate; a finding can be marked resolved only with the exact
new evidence IDs that demonstrate resolution. Never manufacture a union from
agent prose, and never rewrite a previous immutable candidate.

For iOS/iPadOS Apple checks, use only the active confirmed contract and actual
Host in-scope evidence. A screenshot or synthetic pointer event can support a
layout or state claim, but cannot establish physical smoothness; report that
limitation instead of inventing a DOM `motion_required` field. Treat a failed
required check as a blocker only when the confirmed contract requires it, not
because of a subjective style preference.

For motion-heavy flows, capture two frames or observations across time so the
report can distinguish a static render from a broken transition.

## Sources

Reviewed: 2026-09-06

- LingXi Local Apps handoff: `docs/local-apps/HANDOFF.md`
