//! Build-only crate: compiles vendored libcap (`third_party/libcap`) to a
//! static archive for Android targets and emits `cargo:rustc-link-lib=static=cap`.
//! No Rust API — minijail's C code is the consumer.
#![forbid(unsafe_code)]
