# Dependency source review gate

`cargo audit --deny warnings` and `cargo deny check` check the complete locked
all-features dependency graph. `deny.toml` has no advisory ignores.

`cargo vet check --locked` separately requires `safe-to-deploy` source review.
The store imports published Mozilla, Google and Bytecode Alliance reviews.
It has no generated exemptions, publisher-trust rules or claimed local audits.
The empty local `audits.toml` is intentional until a real review is recorded.

On 2026-09-29 the locked check reports 608 unvetted dependencies. Published
reviews of different versions do not certify these locked versions. Resolving
that gate requires source reviews (including justified version deltas) or
existing trustworthy audit records; a vulnerability scan is not a substitute.

Commands (cargo-vet 0.10.2):

```sh
cargo vet regenerate imports
cargo vet check --locked
cargo vet inspect <package> <version>
# After an actual review:
cargo vet certify <package> <version> --criteria safe-to-deploy
```

The imports lock file pins the imported evidence used by CI. Do not generate
exemptions or use an `|| echo` fallback to turn a failed review into success.
