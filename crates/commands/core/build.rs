//! Capture runtime revision separately from the host identity used by `/version`.

#[path = "../../build-support/git_metadata.rs"]
mod git_metadata;

fn main() {
    git_metadata::emit();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../../build-support/git_metadata.rs");
}
