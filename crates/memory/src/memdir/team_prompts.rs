//! Team-prompt enumeration. Delegates to `memdir::scan` with a
//! team-only root so the same age + size filters apply.

use crate::memdir::paths::MemdirRoots;
use crate::memdir::scan::scan_memdir_at;
use protocol::MemoryEntry;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Enumerate the team prompts directory under `team_dir`.
///
/// Empty `Vec` when `team_dir` does not exist (caller distinguishes
/// disabled-via-settings from missing-on-disk).
///
/// # Errors
///
/// Forwards `std::io::Error` from the underlying scan.
pub fn collect_team_prompts(team_dir: &Path) -> std::io::Result<Vec<MemoryEntry>> {
    collect_team_prompts_at(team_dir, SystemTime::now())
}

/// Test-friendly variant taking an explicit `now`.
///
/// # Errors
///
/// Forwards `std::io::Error` from the underlying scan.
pub fn collect_team_prompts_at(
    team_dir: &Path,
    now: SystemTime,
) -> std::io::Result<Vec<MemoryEntry>> {
    let roots = MemdirRoots {
        user_memdir: PathBuf::from("/dev/null"),
        session_memdir: PathBuf::from("/dev/null"),
        team_memdir: Some(team_dir.to_path_buf()),
    };
    Ok(scan_memdir_at(&roots, now)?.entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn missing_dir_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let v = collect_team_prompts(&tmp.path().join("nope")).unwrap();
        assert!(v.is_empty());
    }

    #[test]
    fn populated_dir_returns_entries_tagged_team() {
        use protocol::MemoryEntryTier;
        let tmp = TempDir::new().unwrap();
        let team = tmp.path().join("team");
        fs::create_dir_all(&team).unwrap();
        fs::write(team.join("a.md"), b"alpha\n").unwrap();
        let entries = collect_team_prompts(&team).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].tier, MemoryEntryTier::Team);
    }
}
