//! Private one-invocation worker plus a private crash-safe shell guardian role.
//! Neither role exposes settings, credentials refresh or a public RPC server.

fn main() {
    use platform_posix::process::team_guardian;
    if team_guardian::is_team_guardian_invocation() {
        // Preflight stays single-threaded: validate the helper's clean launch
        // and close inherited descriptors before constructing a Tokio runtime.
        if team_guardian::prepare_team_guardian_entry().is_err() {
            std::process::exit(70);
        }
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => std::process::exit(70),
        };
        let result = runtime.block_on(team_guardian::run_team_guardian());
        runtime.shutdown_timeout(std::time::Duration::from_millis(100));
        std::process::exit(if result.is_ok() { 0 } else { 70 });
    }
    std::process::exit(harness_runtime::desktop::team_process::worker_main());
}
