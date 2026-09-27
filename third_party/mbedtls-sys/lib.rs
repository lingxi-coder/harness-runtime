//! mbedTLS C build seam.
//!
//! This crate has no Rust API — it exists only to cc-build the three vendored
//! mbedTLS static archives (`build.rs`) and to *declare their linkage* so the
//! consumers (libgit2-sys / libssh2-sys) resolve their `mbedtls_*` references.
//!
//! The linkage MUST live here, not in `build.rs`, because the build script's
//! `cargo:rustc-link-lib` directives are attached to this crate and rustc
//! prunes an otherwise-empty, unreferenced crate (and its native libs) from the
//! final link — producing ~96 undefined `mbedtls_*` symbols. A `#[link]` block
//! plus a `#[used]` anchor that references a crypto symbol keeps this crate, and
//! its archives, in the final link.
//!
//! Order matters under single-pass static resolution: a definition must follow
//! its references, so crypto comes LAST (tls -> x509 -> crypto). `-bundle`
//! passes each `.a` straight to the binary link rather than burying it in this
//! crate's (prunable) rlib.

#![allow(dead_code)]

// Declared tls -> x509 -> crypto so crypto's definitions sit last on the link
// line. `-bundle` keeps each archive out of this rlib; the search path is
// emitted by build.rs.
#[link(name = "mbedtls", kind = "static", modifiers = "-bundle")]
#[link(name = "mbedx509", kind = "static", modifiers = "-bundle")]
#[link(name = "mbedcrypto", kind = "static", modifiers = "-bundle")]
extern "C" {
    fn mbedtls_version_get_number() -> u32;
}

/// Link anchor re-exported to the consumers (libgit2-sys / libssh2-sys). They
/// hold a `#[used]` reference to this so rustc keeps `mbedtls-sys` — and its
/// `#[link]`-declared archives — in the final link. The value is a pointer to a
/// real mbedTLS crypto symbol; it is never meant to be called.
#[doc(hidden)]
#[allow(non_upper_case_globals)]
pub static mbedtls_link_anchor: unsafe extern "C" fn() -> u32 = mbedtls_version_get_number;
