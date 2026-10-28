//! Native helper for the desktop typed-Bash Windows integration test.
#[cfg(windows)]
mod windows {
    use lingxi_core::host::BackgroundExitSink;
    use mobile_linux_api::ProcessError;
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use tokio::io::AsyncWriteExt;

    struct Sink(PathBuf);
    #[async_trait::async_trait]
    impl BackgroundExitSink for Sink {
        async fn append_output(&self, _: &str, text: &str) -> Result<(), ProcessError> {
            let mut file = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&self.0)
                .await
                .map_err(|e| ProcessError::Io(e.to_string()))?;
            file.write_all(text.as_bytes())
                .await
                .map_err(|e| ProcessError::Io(e.to_string()))
        }
        async fn on_exit(&self, _: &str, _: Option<i32>) {}
    }
    fn sink(path: &Path) -> Arc<dyn BackgroundExitSink> {
        Arc::new(Sink(path.to_path_buf()))
    }

    pub async fn run() {
        if platform_windows::process::supervisor::is_supervisor_invocation() {
            if let Some(path) = std::env::var_os("LINGXI_TEST_SUPERVISOR_PID_FILE") {
                std::fs::write(path, std::process::id().to_string()).unwrap();
            }
            platform_windows::process::supervisor::run_supervisor(sink)
                .await
                .unwrap();
            return;
        }
        let mut args = std::env::args_os();
        let _ = args.next();
        let role = args.next().unwrap();
        let directory = PathBuf::from(args.next().unwrap());
        if role == "--child" {
            use std::io::Write as _;
            writeln!(std::io::stdout(), "typed-fixture-before-stop").unwrap();
            std::io::stdout().flush().unwrap();
            std::fs::write(directory.join("child.pid"), std::process::id().to_string()).unwrap();
            std::fs::write(directory.join("child.ready"), "ready").unwrap();
            let limit = std::time::Instant::now() + Duration::from_secs(90);
            while !directory.join("release").exists() && std::time::Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = writeln!(std::io::stdout(), "typed-fixture-tail");
            return;
        }
        assert_eq!(role, "--leader");
        let _child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--child")
            .arg(&directory)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let limit = std::time::Instant::now() + Duration::from_secs(20);
        while !directory.join("child.ready").exists() {
            assert!(std::time::Instant::now() < limit, "child failed to start");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() {
    windows::run().await;
}

#[cfg(not(windows))]
fn main() {
    panic!("Windows-only test helper");
}
