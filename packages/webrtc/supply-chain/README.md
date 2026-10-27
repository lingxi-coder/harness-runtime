# WebRTC dependency source-review gate

`cargo vet check --locked` in `packages/webrtc` checks this independent
workspace and its lockfile, using separately pinned published audit records.
The `nix` policy requires source review of the crates.io 0.31.3 baseline.
For a path dependency, cargo-vet cannot certify local changes; the iOS cfg
patch is separately checked against the upstream archive by
`vendor/audit-nix-upstream.py`. A registry advisory scan does not replace a
source review.

This store has no exemptions, publisher-trust rules, or claimed local audits.
When updating the imported records, refresh this store and the root workspace
store together, review the resulting changes, and retain their locked records.

On 2026-10-26, with WebRTC Cargo.lock SHA-256
`af848e9d12e4db93eaf06327f34f9c02f085391721fcde076a7f5215f0bdbbd4`,
`cargo vet check --locked` reports 320 unvetted package versions: 317 require
`safe-to-deploy` and 3 require `safe-to-run`. They need genuine source reviews
or applicable trusted named audit records. Do not generate exemptions or turn a
failed review into success with a fallback command.
