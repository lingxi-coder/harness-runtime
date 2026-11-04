//! Fresh private Team connector host; communicates only through parent-owned pipes.

#[cfg(target_os = "macos")]
fn main() {
    std::process::exit(harness_runtime::desktop::team_connector::host_main());
}
#[cfg(not(target_os = "macos"))]
fn main() {
    std::process::exit(78);
}
