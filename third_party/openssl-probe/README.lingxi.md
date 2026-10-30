# Local openssl-probe dependency

Based on the published `openssl-probe` 0.2.2 crate, archive SHA-256
`7e036b411754132fd6b74e94b74e9a13c5cf606fa93d1885c35860d3060820b1`,
upstream Git revision `f5964f9e8994251dea150d3e82fcabca8433731c`.
The original crate metadata, MIT and Apache-2.0 licenses, and upstream sources
are retained. `src/lib.rs` and `Cargo.toml` contain the downstream changes.

The downstream implementation treats the presence of either OpenSSL
certificate environment variable as an explicit trust configuration. It
preserves the original environment values, parses directory lists using the
platform's path-list conventions, and does not append system certificate roots
to explicit settings. Invalid or empty explicit settings remain unchanged and
do not trigger default discovery. With neither variable present, system
certificate locations are discovered as before. The initializer's existing
unsafe environment-mutation contract remains in force.

The regression test uses a separate single-threaded child process for every
environment configuration, so it never mutates the test driver's environment.
