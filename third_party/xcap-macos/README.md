# Vendored macOS-only xcap

This directory is a macOS-only source copy of the published
[xcap 0.9.8 crate](https://crates.io/crates/xcap/0.9.8). Its upstream Git
revision is `ef9e2a3892c8b972b5964aece131192d5777c660`, as recorded in the
archive's `.cargo_vcs_info.json`. The published `xcap-0.9.8.crate` archive
has SHA-256
`da270fd7c8581d43d731cb690bcc791fc0a7b3bc96e131a4b6552d0379640a9e`.
The included Apache-2.0 license, original manifest and upstream README come
from that archive.

All files in `src/macos/` and the shared `monitor.rs`, `window.rs`, and
`video_recorder.rs` are unchanged from the archive. The local modifications
are limited to:

- Removing other platform modules, their error variants and dependencies, the
  Windows-specific feature, and the OHOS build script.
- Declaring this package macOS-only in `src/lib.rs`.
- Reducing `Cargo.toml` to the common and macOS dependencies. The full
  original is preserved as `Cargo.toml.orig`.

Harness uses xcap only from its macOS computer-control dependency. Future
upgrades should compare the macOS source and behavior to the published
archive, update the checksum and Cargo lock, and validate screen capture and
permissions on macOS. This is not a cross-platform xcap distribution.
