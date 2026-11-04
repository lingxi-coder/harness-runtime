//! Capture runtime revision separately from the host identity used by `/version`.

#[path = "../build-support/git_metadata.rs"]
mod git_metadata;

fn main() {
    git_metadata::emit();
    println!(
        "cargo:rustc-env=HARNESS_CD_CONFIRM_PROMPT=? This moves the session's working directory and write access there, and loads project configuration ({}, settings) from that location.",
        branding::MEMORY_FILE
    );
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../build-support/git_metadata.rs");
}
