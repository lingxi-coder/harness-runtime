//! Cross-process file lock with PID-liveness detection (A9).
//!
//! Stale locks (whose holder PID has exited) are silently overridden so that
//! a crashed scheduler does not block future ticks indefinitely.

use lingxi_core::host::FileSystem;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;

/// Failure modes for [`try_acquire_lock`] / [`release_lock`].
#[derive(Debug, Clone, Error)]
pub enum CronLockError {
    /// Another live process currently owns this job's lock.
    #[error("lock held by live process {pid}")]
    HeldByLivePid {
        /// PID of the live holder.
        pid: u32,
    },
    /// Underlying file-system I/O error.
    #[error("io: {0}")]
    Io(String),
    /// Failed to deserialize a [`LockRecord`] from disk.
    #[error("parse: {0}")]
    Parse(String),
}

/// On-disk lock contents written by [`try_acquire_lock`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockRecord {
    /// Process ID of the holder.
    pub pid: u32,
    /// Wall-clock time when the lock was acquired.
    pub acquired_at: std::time::SystemTime,
    /// Cron job identifier the lock protects.
    pub job_id: String,
}

/// Try to take the per-job lock.
///
/// - Returns `Ok(())` if we successfully wrote our PID into the lock file
///   (either no prior holder, or the prior holder was stale).
/// - Returns [`CronLockError::HeldByLivePid`] when another live process holds
///   the lock.
/// - Returns [`CronLockError::Io`] on filesystem failure.
pub async fn try_acquire_lock(
    fs: Arc<dyn FileSystem>,
    lock_path: &Path,
    our_pid: u32,
    job_id: &str,
    pid_is_alive: impl Fn(u32) -> bool,
) -> Result<(), CronLockError> {
    let state_dir = lock_path
        .parent()
        .ok_or_else(|| CronLockError::Io("lock path has no parent".into()))?;
    let project_root = state_dir
        .parent()
        .ok_or_else(|| CronLockError::Io("lock path has no project root".into()))?;
    if state_dir.file_name() != Some(std::ffi::OsStr::new(branding::DOT_DIR)) {
        return Err(CronLockError::Io(
            "lock path is outside the project state directory".into(),
        ));
    }
    let relative = Path::new(branding::DOT_DIR).join(
        lock_path
            .file_name()
            .ok_or_else(|| CronLockError::Io("lock path has no file name".into()))?,
    );

    if let Ok(content) = fs
        .read_file_rooted_no_follow(project_root, &relative)
        .await
        .map(|c| c.content)
    {
        if let Ok(existing) = serde_json::from_str::<LockRecord>(&content) {
            if existing.pid != our_pid && pid_is_alive(existing.pid) {
                return Err(CronLockError::HeldByLivePid { pid: existing.pid });
            }
        }
    }

    let record = LockRecord {
        pid: our_pid,
        acquired_at: std::time::SystemTime::now(),
        job_id: job_id.to_string(),
    };
    let body = serde_json::to_string(&record).expect("serialize lock record");
    fs.write_file_rooted_atomic(project_root, &relative, &body)
        .await
        .map_err(|e| CronLockError::Io(e.to_string()))?;
    Ok(())
}

/// Drop the per-job lock file (best-effort).
pub async fn release_lock(fs: Arc<dyn FileSystem>, lock_path: &Path) -> Result<(), CronLockError> {
    let state_dir = lock_path
        .parent()
        .ok_or_else(|| CronLockError::Io("lock path has no parent".into()))?;
    let project_root = state_dir
        .parent()
        .ok_or_else(|| CronLockError::Io("lock path has no project root".into()))?;
    if state_dir.file_name() != Some(std::ffi::OsStr::new(branding::DOT_DIR)) {
        return Err(CronLockError::Io(
            "lock path is outside the project state directory".into(),
        ));
    }
    let relative = Path::new(branding::DOT_DIR).join(
        lock_path
            .file_name()
            .ok_or_else(|| CronLockError::Io("lock path has no file name".into()))?,
    );
    fs.delete_file_rooted_no_follow(project_root, &relative)
        .await
        .map_err(|e| CronLockError::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    // Full mock-fs based tests in test-harness/tests/cron_lock.rs.
}

/// Claude Code 2.1.270 project scheduler ownership record.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchedulerLease {
    /// Owner conversation identity.
    pub session_id: String,
    /// Owner process.
    pub pid: u32,
    /// Process birth token protects against PID reuse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_start: Option<String>,
    /// Acquisition time in epoch milliseconds.
    pub acquired_at: u64,
}

/// Acquire the project-wide legacy cron leader lease. Independent creator
/// sessions still run their own durable tasks while another leader is alive.
pub async fn acquire_scheduler_lease(
    fs: &dyn FileSystem,
    root: &Path,
    identity: &str,
    now_ms: u64,
) -> Result<bool, CronLockError> {
    let _guard = crate::lock_cron_file().await;
    let _file_guard = crate::tasks_file::lock_scheduled_tasks(fs, root)
        .await
        .map_err(|error| CronLockError::Io(error.to_string()))?;
    let path = Path::new(".claude").join(crate::tasks_file::SCHEDULED_TASKS_LOCK);
    let prior = fs.read_file_rooted_no_follow(root, &path).await;
    let mut replace_owned = false;
    if let Ok(content) = &prior {
        if let Ok(previous) = serde_json::from_str::<SchedulerLease>(&content.content) {
            if previous.session_id == identity && previous.pid == std::process::id() {
                return Ok(true);
            }
            replace_owned = previous.session_id == identity;
            if previous.session_id != identity && crate::scheduler::pid_alive_check(previous.pid) {
                let alive = previous.proc_start.as_deref().is_none_or(|expected| {
                    lingxi_core::host::live_sessions::process_start_identity(previous.pid)
                        .is_none_or(|actual| actual == expected)
                });
                if alive {
                    return Ok(false);
                }
            }
        }
    }
    if !replace_owned {
        if prior.is_ok() {
            fs.delete_file_rooted_no_follow(root, &path)
                .await
                .map_err(|error| CronLockError::Io(error.to_string()))?;
        }
        match fs.create_new_file_rooted_no_follow(root, &path).await {
            Ok(()) => {}
            Err(lingxi_core::host::FsError::AlreadyExists(_)) => return Ok(false),
            Err(error) => return Err(CronLockError::Io(error.to_string())),
        }
    }
    let lease = SchedulerLease {
        session_id: identity.to_owned(),
        pid: std::process::id(),
        proc_start: lingxi_core::host::live_sessions::process_start_identity(std::process::id()),
        acquired_at: now_ms,
    };
    let body =
        serde_json::to_string(&lease).map_err(|error| CronLockError::Io(error.to_string()))?;
    fs.write_file_rooted_atomic(root, &path, &body)
        .await
        .map_err(|error| CronLockError::Io(error.to_string()))?;
    Ok(true)
}

/// Release only the lease owned by this conversation.
pub async fn release_scheduler_lease(fs: &dyn FileSystem, root: &Path, identity: &str) {
    let _guard = crate::lock_cron_file().await;
    let Ok(_file_guard) = crate::tasks_file::lock_scheduled_tasks(fs, root).await else {
        return;
    };
    let path = Path::new(".claude").join(crate::tasks_file::SCHEDULED_TASKS_LOCK);
    let Ok(content) = fs.read_file_rooted_no_follow(root, &path).await else {
        return;
    };
    let Ok(previous) = serde_json::from_str::<SchedulerLease>(&content.content) else {
        return;
    };
    if previous.session_id == identity {
        let _ = fs.delete_file_rooted_no_follow(root, &path).await;
    }
}
