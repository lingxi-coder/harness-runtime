//! Public path to the `mock_lsp_server` binary so tests can spawn it.
//!
//! At build time cargo emits the binary at
//! `$CARGO_TARGET_DIR/debug/mock_lsp_server` (or the equivalent for the
//! current profile/triple). We expose [`mock_lsp_server_path`] which
//! returns that path, panicking if the binary has not been built (which
//! `cargo test -p lingxi-lsp --test spawn_handshake_test` ensures via the
//! `mock_lsp_server` `[[bin]]` target on `lingxi-test-harness`).

use std::path::PathBuf;

/// Return the path to the compiled `mock_lsp_server` binary.
///
/// # Panics
/// Panics when the binary was not built. To ensure it's built, the test
/// harness invokes `cargo build -p lingxi-test-harness --bin mock_lsp_server`
/// before spawning.
#[must_use]
pub fn mock_lsp_server_path() -> PathBuf {
    // The test executable reveals the actual Cargo profile/triple directory,
    // including configured target directories that are not in the runtime env.
    let executable = std::env::current_exe().expect("current test executable");
    let profile_dir = executable.parent().expect("test executable directory");
    let profile_dir = if profile_dir.file_name().is_some_and(|name| name == "deps") {
        profile_dir.parent().expect("Cargo profile directory")
    } else {
        profile_dir
    };
    let exe_name = if cfg!(windows) {
        "mock_lsp_server.exe"
    } else {
        "mock_lsp_server"
    };
    let candidate = profile_dir.join(exe_name);
    if candidate.is_file() {
        return candidate;
    }
    panic!(
        "mock_lsp_server binary not found at {}. Run `cargo build -p lingxi-test-harness --bin mock_lsp_server` with the same target/profile first.",
        candidate.display()
    );
}
