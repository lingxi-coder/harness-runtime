//! Private one-invocation worker. No CLI options, settings or public RPC server.

fn main() {
    std::process::exit(harness_runtime::desktop::team_process::worker_main());
}
