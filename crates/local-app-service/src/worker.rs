//! The runtime a Local App's long-lived tasks run on.
//!
//! A profile is cached process-wide, but the tasks it owns (the static server
//! behind a running app, the full-runtime exit watch) used to be spawned onto
//! the AMBIENT runtime. For the first engine that is a runtime the next
//! reconnect or project switch drops while the cached profile survives, so a
//! `Running` entry would outlive the socket it describes. Anchoring them here
//! anchors them all: the runtime lives as long as the process.

use std::sync::OnceLock;

/// A handle on the process-wide runtime Local App tasks are spawned onto.
pub fn worker_runtime() -> &'static tokio::runtime::Handle {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .thread_name("lingxi-local-apps")
                .build()
                .expect("build the local-app worker runtime")
        })
        .handle()
}
