use std::path::PathBuf;
use std::process::Command;

/// Return the runnable stdio MCP fixture, building it on first use.
///
/// Cargo compiles a workspace binary as a test harness for `--all-targets`,
/// which does not create `target/<profile>/mock_stdio_mcp`. Build the actual
/// binary here so each integration test is hermetic and does not depend on a
/// manually executed prerequisite command.
pub fn mock_stdio_mcp_bin() -> PathBuf {
    let mut workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    workspace.pop();
    workspace.pop();
    workspace.pop();

    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    // Keep the runnable fixture outside the workspace target directory used
    // by the parent `cargo test --all-targets`. The parent invocation may
    // replace the plain binary with a test-harness artifact while integration
    // test processes are running in parallel.
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| workspace.join("target"));
    let fixture_target = target_dir.join("test-fixtures");
    let binary = fixture_target
        .join(profile)
        .join(format!("mock_stdio_mcp{}", std::env::consts::EXE_SUFFIX));
    if binary.is_file() {
        return binary;
    }

    let mut command = Command::new(env!("CARGO"));
    command
        .current_dir(&workspace)
        .env("CARGO_TARGET_DIR", &fixture_target)
        .args(["build", "-p", "mock_stdio_mcp", "--bin", "mock_stdio_mcp"]);
    if !cfg!(debug_assertions) {
        command.arg("--release");
    }
    let output = command.output().expect("spawn cargo for MCP fixture");
    assert!(
        output.status.success(),
        "failed to build MCP fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        binary.is_file(),
        "cargo reported success but fixture binary is missing at {binary:?}"
    );
    binary
}
