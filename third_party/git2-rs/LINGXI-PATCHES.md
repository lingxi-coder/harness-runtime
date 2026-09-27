# Local patches to vendored git2-rs

This is a vendored copy of `git2` + `libgit2-sys` (pinned: git2 0.21.0,
libgit2-sys 0.18.5+1.9.4 bundling libgit2 1.9.4). It carries LingXi-local
patches that **must be re-applied on any re-vendor / version bump**:

## 1. rustc-1.82 compatibility (commit fc5543b1, P4a/T3)

git2 0.21.0 uses the bare inherent `str::from_utf8(...)` (stabilized in Rust
1.84) in 63 sites across 28 files, but this workspace pins **rustc 1.82.0**, so
the upstream source does not compile here. Every `str::from_utf8` was rewritten
to the free function `std::str::from_utf8` (ancient, stable — semantically
identical, zero behavior change).

Re-apply after any re-vendor: `rg '[^:]str::from_utf8' git2/src` should return
nothing; if it does, qualify each to `std::str::from_utf8`. (Or bump the
workspace rustc pin to >= 1.84 and drop this patch.)

## libssh2-sys (G7a)

To enable the SSH transport, `libssh2-sys` is vendored into
`third_party/libssh2-sys/` (libssh2-sys 0.3.1 — the version that resolves from
the declared `^0.3.0` constraint — bundling the libssh2 C sources under
`third_party/libssh2-sys/libssh2/`). The `.cargo_vcs_info.json` / `.cargo-ok`
/ checksum / `Cargo.toml.orig` files were stripped from the copy.

`libgit2-sys/Cargo.toml`'s `[dependencies.libssh2-sys]` was repointed from
`version = "0.3.0"` to `path = "../../libssh2-sys"` (kept `optional = true`).
**Re-apply this repoint on any re-vendor / version bump.**

libssh2's crypto backend is **mbedTLS** (`LIBSSH2_MBEDTLS`) as of M-a — see the
"## mbedTLS swap (M-a)" section below. (Historically it was `LIBSSH2_OPENSSL`.)

rustc-1.82 compat: libssh2-sys's Rust sources (`lib.rs`, `build.rs`) use no bare
`str::from_utf8` — no qualification patch needed (unlike git2). Verify on
re-vendor: `rg '[^:]str::from_utf8' third_party/libssh2-sys/*.rs` should be empty.

## TLS backend

Built with `vendored-libgit2 + https + ssh` — TLS + crypto backend is **mbedTLS**
(M-a swap; see below). `vendored-openssl` is dropped from `tools/git-mobile`.

## mbedTLS swap (M-a)

OpenSSL is fully replaced by **mbedTLS 3.6.2 LTS** for libgit2 TLS+SHA256 AND
libssh2 crypto. mbedTLS is vendored at `third_party/mbedtls/` (library `*.c` +
private `*.h`, `include/mbedtls`, `include/psa`, LICENSE; everest/p256-m 3rdparty
dropped — off in stock config) and cc-built by `third_party/mbedtls-sys/` (a
`links="mbedtls"` seam compiling the 3 standard libs mbedcrypto/mbedx509/mbedtls,
exporting `DEP_MBEDTLS_INCLUDE`, link order tls→x509→crypto, NDK API floor 29).

**Linkage — the subtle part (was broken; fixed M-a follow-up).** `mbedtls-sys`
has an empty (doc-only) `lib.rs`, so no Rust code references it. If the archives
are linked the ordinary way (`cc::Build::compile` auto-emitting
`cargo:rustc-link-lib=static=…`, i.e. `+bundle`), rustc bundles each `.a` into
`mbedtls-sys`'s rlib and then **prunes that unreferenced rlib from the final
link** — the mbedTLS objects vanish and the consumers' ~96 `mbedtls_*` C
references go undefined. The Android `.so` link tolerates undefined symbols so
`cargo ndk` *appeared* to pass; the host `cargo test -p tool-git-mobile` (a real
`--no-undefined` link) is the honest signal and failed. Fix:
- `mbedtls-sys/build.rs` suppresses cc's auto-emit (`cargo_metadata(false)`) and
  emits only the link-search path; the archive `name`/`kind=static`/
  `modifiers="-bundle"` are declared via `#[link(..)]` in `mbedtls-sys/lib.rs`
  (order tls→x509→crypto so crypto's defs sit last under single-pass
  resolution), with a `pub static mbedtls_link_anchor` pointing at a real crypto
  symbol.
- Each consumer (`libgit2-sys/lib.rs` gated by `https`, `libssh2-sys/lib.rs`)
  holds a `#[used] static … = mbedtls_sys::mbedtls_link_anchor;`. This Rust-level
  reference keeps `mbedtls-sys` (and its `-bundle` archives) reachable, so rustc
  passes them to the final binary link. `+whole-archive` was deliberately NOT
  used (it defeats dead-stripping / bloats the binary).

**Patches (re-apply ALL on any re-vendor / version bump):**

1. `libgit2-sys/build.rs` — `if https` non-Win/non-Apple arm:
   `GIT_OPENSSL`→`GIT_MBEDTLS`, include `DEP_MBEDTLS_INCLUDE` (not
   `DEP_OPENSSL_INCLUDE`). SHA-256 arm: `GIT_SHA256_OPENSSL`→`GIT_SHA256_MBEDTLS`,
   compile `util/hash/mbedtls.c` (not `openssl.c`). `streams/mbedtls.c` is already
   built by `add_c_files(streams)`; `streams/tls.c` dispatches to
   `git_mbedtls_stream_new` under `#elif GIT_MBEDTLS` (verified).
2. `libgit2-sys/lib.rs` — `openssl_init()` reduced to an unconditional no-op (no
   `openssl_sys::init()`; the `#[cfg]` split removed). mbedTLS self-inits.
3. `libgit2-sys/Cargo.toml` — `https = ["mbedtls-sys"]` (was `["openssl-sys"]`);
   `vendored-openssl = []` (no-op alias); added `[dependencies.mbedtls-sys]`
   (path `../../mbedtls-sys`, optional, gated by `https`); removed the
   `cfg(unix) openssl-sys` dep.
4. `git2/Cargo.toml` — `https` drops `openssl-sys`/`openssl-probe`;
   `vendored-openssl` no-op alias; removed the two `cfg(all(unix,...))`
   openssl-probe/openssl-sys deps.
5. `git2/src/lib.rs` — `openssl_env_init()` (the unix+https openssl-probe arm)
   gated behind `cfg(all(any(), ...))` (dead), and the no-op arm widened to
   `cfg(not(all(any(),...)))` (always) so it covers every target.
6. `libssh2-sys/build.rs` — unix branch `LIBSSH2_OPENSSL`→`LIBSSH2_MBEDTLS`
   (dropped `HAVE_EVP_AES_128_CTR`); `DEP_OPENSSL_INCLUDE`→`DEP_MBEDTLS_INCLUDE`.
   The backend `mbedtls.c` is pulled in by the existing `crypto.c`
   (`#elif LIBSSH2_MBEDTLS #include "mbedtls.c"`) — no file-list change.
7. `libssh2-sys/lib.rs` — removed `extern crate openssl_sys`; unix
   `platform_init()` calls `libssh2_init(0)` (self-init crypto) instead of
   `openssl_sys::init()` + `LIBSSH2_INIT_NO_CRYPTO`.
8. `libssh2-sys/Cargo.toml` — added `[dependencies.mbedtls-sys]` (path
   `../mbedtls-sys`, non-optional — unix always uses mbedTLS); removed both
   `openssl-sys` deps; `openssl-on-win32`/`vendored-openssl` no-op aliases.
9. `tools/git-mobile/Cargo.toml` — removed `"vendored-openssl"` from git2
   features (keep `vendored-libgit2`, `https`, `ssh`).
10. `mbedtls-sys/build.rs` — `cargo_metadata(false)` on each `cc::Build` (suppress
    the `+bundle` auto link-lib); emits only `rustc-link-search`. (See "Linkage"
    above.)
11. `mbedtls-sys/lib.rs` — `#[link(name=…, kind="static", modifiers="-bundle")]`
    for mbedtls/mbedx509/mbedcrypto (order: crypto last) + `pub static
    mbedtls_link_anchor` over a real crypto symbol.
12. `libssh2-sys/lib.rs` + `libgit2-sys/lib.rs` (gated `https`) — `extern crate
    mbedtls_sys;` and `#[used] static _MBEDTLS_LINK_ANCHOR = …mbedtls_link_anchor;`
    to keep the seam (and its archives) in the final link.

**Gate evidence (M-a, re-verified after the linkage fix):**
- Host `cargo test -p tool-git-mobile` (a real `--no-undefined` link) — **links +
  passes, 41/41 tests** (previously: link FAILED with undefined
  `_mbedtls_cipher_finish`, `_mbedtls_ctr_drbg_seed`, `_mbedtls_rsa_pkcs1_*`,
  `_mbedtls_strerror`, … from `libssh2_sys` crypto.o — the bug). `cargo build -p
  tool-git-mobile` builds.
- `cargo ndk` for android-aar links on BOTH arm64-v8a + x86_64.
- `llvm-nm` on each `libandroid_aar.so`: **0** undefined (`U`) `mbedtls_` symbols
  (was ~96 before the fix); ~1031–1032 `mbedtls_*` now DEFINED (`t`), e.g.
  `mbedtls_ssl_handshake`, `mbedtls_x509_crt_parse`, `mbedtls_cipher_finish`,
  `mbedtls_ctr_drbg_seed`. `EVP_*` / `SSL_CTX_new` / `ssl3_*` ABSENT (0).
- `cargo tree -p android-aar | grep -i openssl` → empty (no openssl-sys). NOTE:
  `openssl-probe` v0.1.6 remains in `Cargo.lock` — it is NOT OpenSSL and NOT an
  orphan: it is a pure-Rust CA-path locator pulled transitively by
  `rustls-native-certs` ← `sandbox-runtime` (desktop only; cfg-gated off on the
  macOS host, which is why a host-only `cargo tree -i openssl-probe` prints
  nothing). It has no OpenSSL linkage and is not on the git TLS path; correctly
  left in place.
- The only `OPENSSL_`-prefixed symbols are `OPENSSL_memcpy`/`OPENSSL_memset` —
  local (`t`) inline helpers from the unrelated `ring` crate (a BoringSSL fork
  already in the tree), NOT OpenSSL and NOT on the git TLS path.

**Windows note:** the win32 `LIBSSH2_OPENSSL`/WinHTTP paths still reference
OpenSSL in build.rs, but those features are now no-op aliases with no
openssl-sys dep; Windows is not a target for this mobile stack.

## mbedTLS CA verification (M-b) — NO libgit2 patch required

GATE #2 discovery: this libgit2 version already wires the runtime CA-location
hook for the **mbedTLS** backend, not only OpenSSL. **No `streams/mbedtls.c`
patch was needed** (outcome (a)).

- `src/libgit2/settings.c` `GIT_OPT_SET_SSL_CERT_LOCATIONS` has an `#elif
  defined(GIT_MBEDTLS)` arm that calls `git_mbedtls__set_cert_location(file,
  path)`. So `git2::opts::set_ssl_cert_dir(dir)` (used by
  `tool-git-mobile auth::set_ca_location`) loads the trust store under mbedTLS.
- `streams/mbedtls.c::git_mbedtls__set_cert_location` loads a directory of certs
  with `mbedtls_x509_crt_parse_path(&ca, path)` and installs it via
  `mbedtls_ssl_conf_ca_chain(&config, &ca, NULL)`. The Android system store
  `/system/etc/security/cacerts` (hashed `<hash>.0` PEM files) is exactly this
  directory layout. `GIT_DEFAULT_CERT_LOCATION` stays `NULL` (no build-time
  default) — the host supplies the dir at runtime.
- **Verify mode is fail-closed (verify-required equivalent), UNMODIFIED.** The
  config uses `MBEDTLS_SSL_VERIFY_OPTIONAL` *only* so libgit2 can read the peer
  cert after the handshake (REQUIRED frees it on failure — see the line-104
  comment). `mbedtls_connect` then calls `verify_server_cert`, which checks
  `mbedtls_ssl_get_verify_result` and returns `GIT_ECERTIFICATE` on ANY failure.
  For HTTPS `tool-git-mobile` installs **no** `certificate_check` callback
  (`make_network_callbacks` adds one only for SSH host-key pinning), so
  `transports/httpclient.c::server_connect_stream` returns that
  `GIT_ECERTIFICATE` as a hard connection failure (cert_cb == NULL path), and
  even the PASSTHROUGH branch of `check_certificate` maps `!is_valid` → `-1`.
  There is NO code path that accepts an unverified server cert. This matches the
  OpenSSL backend's contract. We did NOT set VERIFY_NONE/OPTIONAL ourselves and
  did NOT add any accept-any override.

Live good-vs-bad-cert verification against the on-device store is device-only
(PENDING-DEVICE, recorded in M-c). The mechanism is present + verify-required at
the source level here.

## git2-rs: add `opts::set_homedir` (G7 SSH on Android, device-acceptance)

`git2/src/opts.rs` adds a `pub unsafe fn set_homedir<P: IntoCString>(path)` over
`GIT_OPT_SET_HOMEDIR` (mirroring `set_ssl_cert_dir`). Upstream git2 0.21 exposes
`GIT_OPT_SET_HOMEDIR` in `libgit2-sys` but provides no safe wrapper.

**Why:** libgit2 resolves+caches its home directory at init (from `HOME` on
non-Windows). An Android app process has no usable `HOME`, so libssh2`s SSH
transport — which expands `~/.ssh/known_hosts` before host-key verification —
fails with `error loading known_hosts` *before* our pinned `certificate_check`
runs (a MISSING known_hosts file is fine; an UNRESOLVABLE `~` is fatal). Setting
the homedir override fixes it. Consumed by `tool-git-mobile auth::ensure_ssh_homedir`
under its `#[allow(unsafe_code)]` carve-out. Caught by on-device G7 acceptance.

Re-apply after any re-vendor: re-add `set_homedir` to `git2/src/opts.rs` (grep
`fn set_homedir`; if absent, copy the `set_ssl_cert_dir` shape with
`GIT_OPT_SET_HOMEDIR` + a single path arg).

## libssh2 mbedTLS backend: ed25519 — RESOLVED (see "ref10 Ed25519" patch below)

ORIGINAL FINDING (device-acceptance): the mbedTLS backend shipped
`#define LIBSSH2_ED25519 0` — RSA/ECDSA only; ed25519 keys (the modern OpenSSH
default) failed at the publickey signature phase, while host OpenSSH authenticated
fine. mbedTLS itself has no Ed25519 implementation (only X25519/Curve25519 for ECDH).

**RESOLVED** by vendoring ref10 Ed25519 + implementing the mbedTLS-backend
contract — see the "**libssh2: ref10 Ed25519**" patch section below. The flag is
now `LIBSSH2_ED25519 1`; ed25519 userauth + host keys + `curve25519-sha256` KEX
work. Device-proven: the same ed25519 key that failed now clones+pushes over SSH.

## libssh2: ref10 Ed25519 for the mbedTLS backend (restore ssh-ed25519 + curve25519-sha256)

mbedTLS has no Ed25519 (only X25519/Curve25519 for ECDH), so the size-opt swap
dropped `ssh-ed25519`. Fix = vendor the public-domain **ref10** Ed25519 (the
implementation OpenSSH ships) + implement libssh2's mbedTLS-backend crypto
contract on it; X25519 KEX reuses mbedTLS's existing Curve25519.

**Vendored source (provenance):** OpenSSH **9.9p1** consolidated ed25519, under
`third_party/libssh2-sys/libssh2/src/ed25519/`:
- `ed25519.c` — sha256 `445c5c9a1ca83e518eca26ec1874bacce29af63474a8c18490f21b1e86a1e18e` (self-contained ref10: fe/ge/sc/verify inlined; `crypto_sign_ed25519`/`_open`/`_keypair`).
- `crypto_api.h` — sha256 `6e56b26e4689f82a0ed218344de67c55aba55849b78b0a994a94cd49a5f757e3` (types + decls; `randombytes` is a `#define`→`arc4random_buf` macro).
- From `openssh-9.9p1.tar.gz` sha256 `b343fbcdbff87f15b1986e6e15d6d4fc9a7d36066be6b7fb507087ba8f966c02` (OpenBSD/OpenSSH mirrors; github blocked on the dev network).

**Added (LingXi-local, NOT upstream):**
- `ed25519/includes.h` — shim: `#define HAVE_STDINT_H 1` + `<stdint.h>` (both vendored files `#include "includes.h"`; that's the only OpenSSH-build coupling).
- `ed25519/ed25519_glue.c` — defines `int crypto_hash_sha512(...)` via `mbedtls_sha512`. NO `randombytes` (it's the upstream macro; keygen is never called — keys are loaded, not generated).
- `mbedtls.h` — `#define LIBSSH2_ED25519 1`; `libssh2_mbedtls_ed25519_ctx { unsigned char pub[32]; priv[64]; int has_private; }` + `#define libssh2_ed25519_ctx …`; `void _libssh2_ed25519_free(libssh2_ed25519_ctx *)` prototype.
- `mbedtls.c` — `#include "ed25519/crypto_api.h"` + an `#if LIBSSH2_ED25519` section (after the static `_libssh2_mbedtls_ctr_drbg`) implementing all 10 contract functions: `_ed25519_free/_new_public/_sign/_verify/_new_private/_new_private_frommemory` + the two `_sk` parse variants (over ref10), and `_curve25519_new/_gen_k` (over mbedTLS `MBEDTLS_ECP_DP_CURVE25519`).
- `libssh2-sys/build.rs` — `.file("libssh2/src/ed25519/ed25519.c")` + `.file("libssh2/src/ed25519/ed25519_glue.c")`.

**Critical correctness points (for re-apply / review):**
- **ctx alloc:** the ctx is freed by `_libssh2_ed25519_free(ctx)` with NO session → it MUST be libc `calloc`/`free`, NOT session-routed `LIBSSH2_CALLOC`. Buffers RETURNED to libssh2 (signature, pub/priv key bytes, sk `application`/`key_handle`) use `LIBSSH2_ALLOC` (libssh2 frees them).
- **ed25519 sk format:** the OpenSSH 64-byte private field IS ref10's `sk` (seed‖pub) — copy verbatim into `ctx->priv`.
- **sign/verify:** ref10's `crypto_sign_ed25519` emits `sig(64)‖msg`; take the first 64 (detached). Verify builds `sig‖msg` → `crypto_sign_ed25519_open`.
- **curve25519 byte order (the trap):** X25519 is little-endian; libssh2 forms `K` via a big-endian read of the raw output → `mbedtls_mpi_write_binary_le(&R.X,…)` then `mbedtls_mpi_read_binary(*k,…)`. Validated by the RFC 7748 §5.2 KAT.

**Verification done:** RFC 8032 §7.1 + RFC 7748 §5.2 host KATs both pass; `nm -u` on both ABIs shows zero undefined ed25519/curve25519/mbedtls symbols + `libssh2.a` defines the ref10 + `hostkey_method_ssh_ed25519`; stripped `.so` +~170 KB/ABI; host `tool-git-mobile` 45/45; device-proven ed25519 SSH clone+push on a physical arm64 device.

**Re-apply on re-vendor:** re-extract `ed25519.c`+`crypto_api.h` from openssh-9.9p1 (verify the sha256s above); re-add the `includes.h` shim + `ed25519_glue.c`; re-add the two `build.rs` `.file(...)`; re-set `LIBSSH2_ED25519 1` + the ctx/proto in `mbedtls.h`; re-add the `#if LIBSSH2_ED25519` section in `mbedtls.c`. `_sk` is parse-only backend parity (no live FIDO transport).

**Known limit:** the FIDO `sk-ssh-ed25519` `_sk` functions parse the key + metadata for contract parity but live FIDO signing is not wired (needs an authenticator + sk-signing path above libssh2). Not in scope.
