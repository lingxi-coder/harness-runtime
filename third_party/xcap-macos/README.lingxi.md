# macOS-only xcap source

This private dependency retains the macOS implementation and public Rust
facade of [xcap 0.5.2](https://crates.io/crates/xcap/0.5.2). The original
Apache-2.0 license is included in `LICENSE`. `UPSTREAM.json` records the
published crate checksum and the SHA-256 of each retained, unmodified source
file. Upstream Linux and Windows source files remain solely so Rust formatting
can resolve every module declared in the unchanged facade. Their dependency
lists are omitted, and the only consumer declares this dependency under
`cfg(target_os = "macos")`. This copy is not a general cross-platform fork.

The original `src/lib.rs` and its target-specific module declarations are
preserved exactly. The package must not be built or used as a Linux or Windows
implementation. When an audited
upstream release can be used without the unused target dependencies, prefer
replacing this private copy with that release.

Run `python3 third_party/xcap-macos/verify-upstream.py` to check the retained
file set and hashes. Supplying `--archive /path/to/xcap-0.5.2.crate` also
checks the published archive checksum and each retained file against the
archive. Keep the separate upstream advisory audit and cargo-vet policy for
this copy: a Cargo path dependency alone is not checked by cargo-audit as the
published crate.
