# Vendored Mbed TLS build

`../mbedtls/{include,library}` and `../mbedtls/LICENSE` are the corresponding
277 files from the official Mbed TLS 3.6.7 release tarball:

- Release: <https://github.com/Mbed-TLS/mbedtls/releases/tag/mbedtls-3.6.7>
- Artifact: `mbedtls-3.6.7.tar.bz2`
- SHA-256: `a7e8bcbec0e6f761b4af24f25677626b35f762f68eef79c08677a363212d11f6`

The archive includes other directories that this build does not vendor. The
upstream configuration header is unchanged in the source tree. At build time
`build.rs` copies the headers to `OUT_DIR/lingxi-mbedtls-include`, saves that
header as `mbedtls_config_upstream.h`, and installs `lingxi_config.h` as the
shared `mbedtls_config.h`. It compiles all three native archives against this
tree and exports the same path as `DEP_MBEDTLS_INCLUDE` for C consumers.

The wrapper enables the pthread backend on Unix. It disables upstream's
`MBEDTLS_SELF_TEST`, whose ECP diagnostic counters are process-global and
unsynchronized; the repository has no product calls to Mbed TLS self tests.
This disables those diagnostic APIs, not the production cryptographic features.
On Windows it does not enable the alternate threading backend: libgit2 uses
WinHTTP and libssh2 uses WinCNG, although libssh2's Ed25519 code still uses
Mbed TLS SHA-512. This configuration does not claim general Windows Mbed TLS
multithreading support.

The mutex backend alone does not establish all global lifecycle guarantees.
Upstream's default ciphersuite list has lazy global initialization, and PSA
initialization error handling may free global state. Their callers still need
appropriate lifetime and initialization coordination; this build change is not
evidence that concurrent CA updates or all Mbed TLS consumers are safe.
