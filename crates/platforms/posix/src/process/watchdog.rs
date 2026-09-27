//! Background-shell health signals (Claude Code 2.1.263 `Her` / `jer`).
//!
//! These signals never decide session policy: the task owner decides whether
//! memory pressure may stop its shell. Only the runner's held process is killed.
use std::time::Duration;

pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(5);

pub(super) use platform_api::shell_watchdog::ShellWatchdog;

/// Read an OS pressure signal, never infer pressure from our own RSS. Unsupported
/// kernels and failed reads decline to reap. macOS publishes the memorystatus
/// pressure level; Linux PSI reports actual time when all tasks stalled on memory.
pub(super) async fn memory_pressure() -> bool {
    #[cfg(target_os = "macos")]
    {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            tokio::process::Command::new("/usr/sbin/sysctl")
                .args(["-n", "kern.memorystatus_vm_pressure_level"])
                .kill_on_drop(true)
                .output(),
        )
        .await;
        matches!(result, Ok(Ok(output)) if output.status.success()
            && matches!(String::from_utf8_lossy(&output.stdout).trim(), "2" | "4"))
    }
    #[cfg(target_os = "linux")]
    {
        let Ok(psi) = tokio::fs::read_to_string("/proc/pressure/memory").await else {
            return false;
        };
        psi.lines()
            .filter(|line| line.starts_with("full "))
            .flat_map(str::split_whitespace)
            .filter_map(|field| field.strip_prefix("avg10="))
            .filter_map(|value| value.parse::<f64>().ok())
            .any(|value| value > 0.0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    false
}
