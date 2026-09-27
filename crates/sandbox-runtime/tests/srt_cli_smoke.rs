//! Smoke test for the `srt` CLI binary (P10).
//!
//! Runs the built `srt` binary end to end:
//! - `srt -c 'echo hi'` with the default/empty config → exits 0 and prints `hi`
//!   (on macOS the Seatbelt `sandbox-exec` wrap + exec; on Linux this needs the
//!   bwrap/socat deps, so it is gated to macOS).
//! - `srt` with no command → exit 1 + the "No command specified" error (any
//!   host, no sandbox deps needed).
//!
//! Requires the `cli` feature (the `srt` bin is `required-features = ["cli"]`),
//! so run with `cargo test -p sandbox-runtime --features cli`. When the feature
//! is off the `CARGO_BIN_EXE_srt` env var is absent at compile time and these
//! tests are skipped via the `cfg!` guard below.

// `CARGO_BIN_EXE_srt` is only defined when the `srt` bin is part of the build
// (i.e. the `cli` feature is enabled). Guard the whole module so a default
// `cargo test` (no `cli`) still compiles.
#[cfg(feature = "cli")]
mod cli {
    use std::process::Command;

    fn srt() -> Command {
        Command::new(env!("CARGO_BIN_EXE_srt"))
    }

    /// No command → exit 1 + the exact TS error. Host-independent (fails before
    /// any sandbox work).
    #[test]
    fn no_command_errors_with_exit_1() {
        let out = srt().output().expect("run srt");
        assert!(!out.status.success(), "expected failure exit");
        assert_eq!(out.status.code(), Some(1));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("No command specified. Use -c <command> or provide command arguments."),
            "stderr: {stderr}"
        );
    }

    /// `-c 'echo hi'` with the default config → exit 0 + `hi` on stdout. macOS
    /// wraps with `sandbox-exec` and execs; on Linux this needs bwrap/socat, so
    /// it is macOS-gated.
    #[cfg(target_os = "macos")]
    #[test]
    fn echo_hi_exits_0_and_prints_hi() {
        let out = srt().arg("-c").arg("echo hi").output().expect("run srt");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "expected exit 0; stderr: {stderr}; stdout: {stdout}"
        );
        assert!(stdout.contains("hi"), "stdout: {stdout}; stderr: {stderr}");
    }
}
