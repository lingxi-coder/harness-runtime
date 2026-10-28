# WebRTC dependency source-review gate

`cargo vet check --locked` in `packages/webrtc` checks this independent
workspace and its lockfile. The store imports published records from Mozilla,
Google, Bytecode Alliance, ISRG, Zcash and Embark Studios. Source URLs pin
specific upstream commits; the imports lock records the evidence for this graph.
Imported records retain the original auditor and notes, including named
wildcard audits that attest to a publisher's release process for a specified
period.
The `nix` policy requires source review of the crates.io 0.31.3 baseline.
For a path dependency, cargo-vet cannot certify local changes; the iOS cfg
patch is separately checked against the upstream archive by
`vendor/audit-nix-upstream.py`. A registry advisory scan does not replace a
source review.

This store has no exemptions, publisher-trust rules, or claimed local audits.
The root workspace store is separate. The two graphs can contain the same
package versions, so their unvetted counts must not be added. When updating
imported records, review the effect on both graphs and retain each graph's
locked records. For pinned sources, changing the upstream commit is an explicit
review step.

On 2026-10-27, with WebRTC Cargo.lock SHA-256
`af848e9d12e4db93eaf06327f34f9c02f085391721fcde076a7f5215f0bdbbd4`,
`cargo vet check --locked --frozen` reports 291 unvetted package versions:
288 require `safe-to-deploy` and 3 require `safe-to-run`. They need genuine source reviews
or applicable trusted named audit records. Do not generate exemptions or turn a
failed review into success with a fallback command.
