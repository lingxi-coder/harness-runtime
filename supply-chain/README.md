# Dependency source review gate

`cargo audit --deny warnings` and `cargo deny check` check the locked
dependency graph. `deny.toml` has no advisory ignores. Cargo-audit does not
check path dependencies against the RustSec database, so CI also runs
`third_party/xcap-macos/audit-upstream.py` to verify the vendored source and
audit the corresponding crates.io package.
The independent `packages/webrtc/Cargo.lock` is checked separately with
`cargo audit --deny warnings --file packages/webrtc/Cargo.lock` and the same
deny policy, because it is not part of the root lock.

`cargo vet check --locked` separately requires `safe-to-deploy` source review
for normal and build dependencies and `safe-to-run` for dev-only dependencies.
The store imports published Mozilla, Google, Bytecode Alliance, ISRG, Zcash and
Embark Studios reviews. The additional import URLs pin specific upstream
commits; imported records retain the original reviewer and notes.
It has no generated exemptions, publisher-trust rules or claimed local audits.
The empty local `audits.toml` is intentional until a real review is recorded.

On 2026-10-27, `cargo vet check --locked --frozen` reports 506 unvetted
package versions (490 `safe-to-deploy`, 16 `safe-to-run`) for Cargo.lock SHA-256
`58ea0be446b31d9e31088ad3ca09d28411c5dd11ec8165e8ea3a83eb9963c025`.
The root and independent WebRTC graphs can contain the same package versions,
so their unvetted counts must not be added. Published reviews of different
versions do not certify these locked versions.
Resolving that gate requires source reviews (including justified version
deltas) or existing trustworthy audit records; a vulnerability scan is not a
substitute.

Commands (cargo-vet 0.10.3):

```sh
cargo vet regenerate imports
cargo vet check --locked
cargo vet inspect <package> <version>
# After an actual review:
cargo vet certify <package> <version> --criteria safe-to-deploy
```

The imports lock file pins the imported evidence used by CI. Do not generate
exemptions or use an `|| echo` fallback to turn a failed review into success.
