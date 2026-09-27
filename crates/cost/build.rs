//! Captures the compile-time epoch so [`cost::events::build_age_mins`] can
//! report minutes since the build — the faithful analog of claude-code
//! 2.1.195's `THl()` (`buildAgeMins:THl()` in the `tengu_api_success` payload),
//! which reads a build timestamp baked into the binary at release time.

use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    // Seconds since the Unix epoch at the moment this crate is compiled.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=LINGXI_COST_BUILD_EPOCH_SECS={secs}");
    // Re-run only when the build script itself changes (the timestamp is a
    // build-time artifact; we do not want a stamp churn on every `cargo build`).
    println!("cargo:rerun-if-changed=build.rs");
}
