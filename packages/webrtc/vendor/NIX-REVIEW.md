# nix 0.31.3 source and iOS patch review

The source baseline is the published crates.io `nix-0.31.3.crate`, SHA-256
`cf20d2fde8ff38632c426f1165ed7436270b44f199fc55284c38276f9db47c3d`.
The retained `Cargo.toml`, `build.rs`, MIT `LICENSE`, `README.md`, and all
64 files under `src/` are byte-for-byte identical to the archive except the
four guards described below. Upstream `test/`, `Cargo.lock`, `Cargo.toml.orig`,
and `CHANGELOG.md` were not retained; they are not part of the vendored
production source set. The verifier checks the exact retained inventory and
compares every byte against the checksum-verified archive after applying only
these four context-bound substitutions. It rejects extra source, build scripts,
manifest changes, symlinks, missing files, or any other source delta.

| Patched location | Source review and effect |
| --- | --- |
| `src/sys/socket/mod.rs`, `SockProtocol::KextControl` | Replaces `cfg(apple_targets)` with `cfg(target_os = "macos")` around the enum variant using `libc::SYSPROTO_CONTROL`. The value and API remain the same on macOS. |
| `src/sys/socket/sockopt.rs`, `UtunIfname` | Makes the option using `libc::SYSPROTO_CONTROL` and `libc::UTUN_OPT_IFNAME` macOS-only; the existing `net` feature guard and implementation remain unchanged. |
| `src/sys/socket/sockopt.rs`, private `GetCString` type | Restricts the helper type to macOS alongside its only `UtunIfname` use. |
| `src/sys/socket/sockopt.rs`, private `Get<CString>` implementation | Restricts the helper implementation to macOS with the type; buffer, FFI and error behavior are otherwise unchanged. |

The original `apple_targets` alias includes iOS, macOS, watchOS, tvOS and
visionOS. In the actual nested lock, `libc 0.2.191` declares both referenced
constants only under `cfg(target_os = "macos")`. The upstream guards therefore
expose nonexistent constants to other Apple targets; the delta removes those
four APIs/implementation declarations on non-macOS Apple targets while
preserving the macOS branches and leaving other platforms unaffected. This is
a reviewed compatibility patch, not a claim of device-level execution or a
review of every upstream API. No replacement constants or new dependencies are
introduced.

Cargo metadata is required to resolve the checked vendor manifest: a path
identity in the embedded WebRTC workspace, or the same pinned Harness Git
revision used by the application. A temporary lock audits only the original
registry identity, with no ignored advisories. The existing complete workspace
`cargo deny`/`cargo audit`/`cargo vet` remain independent gates; registry
advisories cannot detect flaws introduced solely by this local delta, which
continues to require source review and supply-chain policy coverage.
