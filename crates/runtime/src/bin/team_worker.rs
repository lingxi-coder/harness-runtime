//! Private major-3 Shared Bot worker. No command, shell-helper or public RPC role.
fn main() {
    std::process::exit(harness_runtime::desktop::team_process::worker_main());
}
