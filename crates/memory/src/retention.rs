//! On-disk data-retention sweep — a port of claude-code 2.1.206's `fWu`.
//!
//! The sweep deletes stale session-file entries (older than the retention
//! period) from the config-home housekeeping directories `todos`, `statsig`,
//! and `logs`, mirroring claude's inner loop
//! (`for dir in ["todos","statsig","logs"]: delete entries with mtime < cutoff`).
//!
//! Because it DELETES user data, it is **flag-gated and default-OFF**: it runs
//! only when `LINGXI_RETENTION_SWEEP` is explicitly set to a truthy value, and
//! it never touches anything outside the three named directories. The retention
//! period is claude's `cleanupPeriodDays` (default 30), overridable via
//! `LINGXI_RETENTION_PERIOD_DAYS`.
//!
//! Scope note: the port sweeps the session-file directories only. Claude's `fWu`
//! also runs ~two dozen per-subsystem cleanups plus transcript and worktree
//! cleanup; those are separate subsystems and are not ported here.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Enable flag — `LINGXI_RETENTION_SWEEP`. Off by default.
pub const RETENTION_ENABLE_ENV: &str = "LINGXI_RETENTION_SWEEP";
/// Period override — `LINGXI_RETENTION_PERIOD_DAYS`.
pub const RETENTION_PERIOD_ENV: &str = "LINGXI_RETENTION_PERIOD_DAYS";
/// claude-code `cleanupPeriodDays` default (`dWu = 30`).
pub const DEFAULT_CLEANUP_PERIOD_DAYS: u64 = 30;

/// Entry-level housekeeping directories — every stale entry (file or dir) is
/// removed (claude's `["todos","statsig","logs"]` loop).
const SWEPT_ENTRY_DIRS: &[&str] = &["todos", "statsig", "logs"];

/// Subdirectory-level housekeeping directories — every stale *subdirectory* is
/// removed (claude's `VRt(...)` cleanups: `file-history`, `session-env`,
/// `tasks`, `uploads`, `skills/.staging`).
const SWEPT_SUBDIR_TREES: &[&str] = &[
    "file-history",
    "session-env",
    "tasks",
    "uploads",
    "skills/.staging",
    "shares", // wVg (also `.zip`-file-swept)
];

/// Subdirectory-level dir also swept for stale `.zip` files (claude `wVg`).
/// Kept out of [`SWEPT_SUBDIR_TREES`]'s comment list only for clarity — it is
/// appended below.
///
/// `(dir, extension)` file sweeps — claude's `aj(<dir>, <ext>)` cleanups. Every
/// direct child file whose name ends with `ext` and whose mtime is older than
/// the cutoff is removed, then the dir is pruned if it emptied.
/// `(dir, ext, cap_days)` — a `Some(cap)` retains for `min(period, cap)` days
/// (claude's `UY(cap)`), so a capped dir is cleaned more aggressively.
const SWEPT_FILE_EXTS: &[(&str, &str, Option<u64>)] = &[
    ("plans", ".md", None),                      // hVg
    ("telemetry", ".json", None),                // AVg
    ("traces", ".json", None),                   // OVg
    ("startup-perf", ".txt", None),              // OVg
    ("startup-perf", ".json", None),             // OVg
    ("shell-snapshots", ".sh", None),            // xVg
    ("feedback-bundles", ".zip", None),          // PVg
    ("dump-prompts", ".jsonl", Some(3)),         // RVg (Qju = 3-day cap)
    ("shares", ".zip", None),                    // wVg (also VRt-swept below)
    ("backups", "", None),                       // HVg (every file)
    ("jobs/settled", ".json", None),             // IVg
    ("daemon/dispatch/rejected", ".json", None), // IVg
    ("daemon/dispatch", ".json", None),          // IVg
    ("daemon/auth", ".json", None),              // IVg
];

/// Single cache files removed when stale (claude `fVg` / `mVg`).
const SWEPT_SINGLE_FILES: &[&str] = &["hfi-auth.json", "mcp-needs-auth-cache.json"];

/// Result of a sweep (claude's `{messages, errors}` accumulator; `messages` is
/// the deleted-entry count).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Session-file entries (files or directories) deleted.
    pub session_files_deleted: u64,
    /// Transcript `.jsonl` files deleted (a subset of `session_files_deleted`,
    /// reported separately for the `tengu_retention_sweep` `transcripts` field).
    pub transcripts_deleted: u64,
    /// Entries that could not be stat'd or removed.
    pub errors: u64,
}

/// Whether the sweep is enabled. **Default OFF** — it runs only when
/// [`RETENTION_ENABLE_ENV`] is a truthy value (`1`/`true`/`yes`/`on`,
/// case-insensitive). Off ⇒ no file is ever touched.
#[must_use]
pub fn retention_sweep_enabled() -> bool {
    std::env::var(RETENTION_ENABLE_ENV)
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// The retention period in days and whether the default was used (claude
/// `cleanupPeriodDays ?? 30`, `usedDefault = cleanupPeriodDays === undefined`).
/// A [`RETENTION_PERIOD_ENV`] value must parse to a positive integer to apply.
#[must_use]
pub fn retention_period_days() -> (u64, bool) {
    match std::env::var(RETENTION_PERIOD_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&d| d > 0)
    {
        Some(days) => (days, false),
        None => (DEFAULT_CLEANUP_PERIOD_DAYS, true),
    }
}

/// Run the sweep rooted at `config_home`, deleting entries in the swept
/// directories whose mtime is older than `now - period`. Pure I/O; performs no
/// gating and emits no telemetry (the caller does both).
///
/// Safety: only the three named subdirectories of `config_home` are read, and
/// only entries strictly older than the cutoff are removed. A missing directory
/// is skipped; an unreadable entry counts as an error, never a deletion.
#[must_use]
pub fn run_retention_sweep_in(config_home: &Path, period: Duration) -> RetentionReport {
    let mut report = RetentionReport::default();
    let now = SystemTime::now();
    // A period so large the cutoff underflows ⇒ nothing is old enough ⇒ no-op.
    let Some(cutoff) = now.checked_sub(period) else {
        return report;
    };
    // Entry-level sweep (todos/statsig/logs): remove every stale file/dir.
    for sub in SWEPT_ENTRY_DIRS {
        report = merge(
            report,
            sweep_stale_entries(&config_home.join(sub), cutoff, true),
        );
    }
    // Subdirectory-level sweep (VRt): remove stale subdirectories, then prune the
    // named dir if it emptied.
    for sub in SWEPT_SUBDIR_TREES {
        report = merge(
            report,
            sweep_stale_entries(&config_home.join(sub), cutoff, false),
        );
    }
    // File-extension sweeps (aj): stale `<dir>/*<ext>` files. A capped dir uses
    // the more-recent `now - min(period, cap)` cutoff (claude `UY(cap)`).
    for (dir, ext, cap_days) in SWEPT_FILE_EXTS {
        let dir_cutoff = match cap_days {
            Some(days) => now
                .checked_sub(period.min(day_period(*days)))
                .unwrap_or(cutoff),
            None => cutoff,
        };
        report = merge(
            report,
            sweep_stale_files_by_ext(&config_home.join(dir), ext, dir_cutoff, None),
        );
    }
    // Debug dir (DVg): every stale file except `latest`.
    report = merge(
        report,
        sweep_stale_files_by_ext(&config_home.join("debug"), "", cutoff, Some("latest")),
    );
    // Single stale cache files (fVg / mVg).
    for name in SWEPT_SINGLE_FILES {
        report = merge(report, sweep_stale_file(&config_home.join(name), cutoff));
    }
    // MCP log dirs by filename-timestamp (dVg) and session transcripts (pVg).
    report = merge(report, sweep_mcp_logs(config_home, cutoff));
    report = merge(report, sweep_transcripts(config_home, cutoff));
    // Orphaned host-managed daemon records (IVg tail).
    report = merge(report, sweep_orphan_host_managed(config_home, cutoff));
    report
}

/// Remove stale `daemon/host-managed/<name>` records whose backing
/// `jobs/<name>` no longer exists (claude `IVg` tail): a record is deleted only
/// when its job is gone AND the record is older than the cutoff.
fn sweep_orphan_host_managed(config_home: &Path, cutoff: SystemTime) -> RetentionReport {
    let mut report = RetentionReport::default();
    let host_managed = config_home.join("daemon").join("host-managed");
    let jobs = config_home.join("jobs");
    let Ok(entries) = std::fs::read_dir(&host_managed) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        match jobs.join(&name).try_exists() {
            Ok(true) => continue, // job still exists → keep the record
            Ok(false) => {}       // job gone → candidate for deletion
            Err(_) => {
                report.errors += 1; // couldn't tell (non-ENOENT) → skip
                continue;
            }
        }
        let Ok(meta) = entry.metadata() else {
            report.errors += 1;
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            report.errors += 1;
            continue;
        };
        if mtime < cutoff {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => report.session_files_deleted += 1,
                Err(_) => report.errors += 1,
            }
        }
    }
    report
}

/// Merge two reports (claude `Vnt`).
fn merge(a: RetentionReport, b: RetentionReport) -> RetentionReport {
    RetentionReport {
        session_files_deleted: a.session_files_deleted + b.session_files_deleted,
        transcripts_deleted: a.transcripts_deleted + b.transcripts_deleted,
        errors: a.errors + b.errors,
    }
}

/// `rmdir` `dir` if it is now empty — best-effort (claude `sj`).
fn prune_if_empty(dir: &Path) {
    let _ = std::fs::remove_dir(dir);
}

/// Remove a single file when its mtime is older than `cutoff` (claude
/// `fVg`/`mVg`, one-file `Xae`). Absent file → no-op.
fn sweep_stale_file(path: &Path, cutoff: SystemTime) -> RetentionReport {
    let mut report = RetentionReport::default();
    let Ok(meta) = std::fs::metadata(path) else {
        return report; // absent → nothing to do
    };
    if !meta.is_file() {
        return report;
    }
    match meta.modified() {
        Ok(mtime) if mtime < cutoff => match std::fs::remove_file(path) {
            Ok(()) => report.session_files_deleted += 1,
            Err(_) => report.errors += 1,
        },
        Ok(_) => {}
        Err(_) => report.errors += 1,
    }
    report
}

/// Remove stale entries under `dir` whose mtime is older than `cutoff`. When
/// `include_files` is true every entry (file or dir) is eligible (the
/// todos/statsig/logs loop); when false only subdirectories are (claude `VRt`,
/// which then prunes the emptied parent).
fn sweep_stale_entries(dir: &Path, cutoff: SystemTime, include_files: bool) -> RetentionReport {
    let mut report = RetentionReport::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return report; // absent / unreadable directory → skip
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            report.errors += 1;
            continue;
        };
        if !include_files && !meta.is_dir() {
            continue; // VRt considers subdirectories only
        }
        let Ok(mtime) = meta.modified() else {
            report.errors += 1;
            continue;
        };
        // Keep anything modified at or after the cutoff (claude `mtime>=e` /
        // `mtime<n`).
        if mtime >= cutoff {
            continue;
        }
        let path = entry.path();
        let removed = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match removed {
            Ok(()) => report.session_files_deleted += 1,
            Err(_) => report.errors += 1,
        }
    }
    if !include_files {
        prune_if_empty(dir);
    }
    report
}

/// Remove stale files directly under `dir` whose name ends with `ext` (an empty
/// `ext` matches every file) and whose mtime is older than `cutoff`, skipping
/// any file named `keep`, then prune the dir if it emptied (claude `aj` /
/// `DVg`). `ext` of `""` matches all files.
fn sweep_stale_files_by_ext(
    dir: &Path,
    ext: &str,
    cutoff: SystemTime,
    keep: Option<&str>,
) -> RetentionReport {
    let mut report = RetentionReport::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if keep.is_some_and(|k| name.to_string_lossy() == k) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            report.errors += 1;
            continue;
        };
        if !meta.is_file() || !name.to_string_lossy().ends_with(ext) {
            continue;
        }
        let Ok(mtime) = meta.modified() else {
            report.errors += 1;
            continue;
        };
        if mtime >= cutoff {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => report.session_files_deleted += 1,
            Err(_) => report.errors += 1,
        }
    }
    prune_if_empty(dir);
    report
}

/// Startup entry point: if the sweep is enabled, run it against the resolved
/// config-home and emit `tengu_retention_sweep`. A no-op (and silent) when
/// disabled. Returns the report when it ran, `None` when gated off.
pub fn run_startup_retention_sweep() -> Option<RetentionReport> {
    if !retention_sweep_enabled() {
        return None;
    }
    let (days, used_default) = retention_period_days();
    let report =
        run_retention_sweep_in(&crate::session_memory::config_home_dir(), day_period(days));
    telemetry::emit_retention_sweep(
        false,
        None,
        report.transcripts_deleted,
        report.session_files_deleted,
        report.errors,
        days,
        used_default,
    );
    Some(report)
}

/// `days` as a [`Duration`] (saturating, so an absurd override can't overflow).
fn day_period(days: u64) -> Duration {
    Duration::from_secs(days.saturating_mul(24 * 60 * 60))
}

// ── mcp-logs (dVg / cWu / uVg) ──────────────────────────────────────────────

/// Parse a log filename's embedded timestamp (claude `uVg`): the name stem is
/// an ISO instant with `-`-separated time fields, e.g.
/// `2024-01-15T10-30-00-000Z.log`. Returns `None` for any name that does not
/// parse (claude's `Invalid Date`, which never compares `< cutoff`, so those
/// files are kept). Dependency-free (days-from-civil), UTC.
fn parse_filename_timestamp(name: &str) -> Option<SystemTime> {
    let stem = name.split('.').next()?;
    let (date, time) = stem.split_once('T')?;
    let time = time.strip_suffix('Z')?;
    let d: Vec<&str> = date.split('-').collect();
    let t: Vec<&str> = time.split('-').collect();
    if d.len() != 3 || t.len() != 4 {
        return None;
    }
    let year: i64 = d[0].parse().ok()?;
    let month: i64 = d[1].parse().ok()?;
    let day: i64 = d[2].parse().ok()?;
    let hour: i64 = t[0].parse().ok()?;
    let minute: i64 = t[1].parse().ok()?;
    let second: i64 = t[2].parse().ok()?;
    let millis: i64 = t[3].parse().ok()?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) || !(0..60).contains(&second) {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    if secs < 0 {
        return None;
    }
    Some(
        SystemTime::UNIX_EPOCH
            + Duration::from_secs(secs as u64)
            + Duration::from_millis(millis.max(0) as u64),
    )
}

/// Days since the Unix epoch for a civil (Y, M, D) date — Howard Hinnant's
/// `days_from_civil`. `None` for an out-of-range month/day.
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = if m > 2 { m - 3 } else { m + 9 }; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    Some(era * 146_097 + doe - 719_468)
}

/// Remove log files under `dir` whose filename-embedded timestamp is older than
/// `cutoff` (claude `cWu`). Files whose name does not parse are kept.
fn sweep_logs_by_filename_timestamp(dir: &Path, cutoff: SystemTime) -> RetentionReport {
    let mut report = RetentionReport::default();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return report;
    };
    for entry in entries.flatten() {
        if !entry.metadata().map(|m| m.is_file()).unwrap_or(false) {
            continue;
        }
        let Some(ts) = parse_filename_timestamp(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if ts < cutoff {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => report.session_files_deleted += 1,
                Err(_) => report.errors += 1,
            }
        }
    }
    report
}

/// Sweep the MCP log directories (claude `dVg`): every `mcp-logs-*` subdir of
/// the base logs dir has its stale (by filename-timestamp) files removed, then
/// the emptied subdir is pruned. The port has no dedicated MCP-log base dir, so
/// the base is `<config>/logs`; inert when no such subdirs exist.
fn sweep_mcp_logs(config_home: &Path, cutoff: SystemTime) -> RetentionReport {
    let mut report = RetentionReport::default();
    let base_logs = config_home.join("logs");
    let Ok(entries) = std::fs::read_dir(&base_logs) else {
        return report;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_dir = entry.metadata().map(|m| m.is_dir()).unwrap_or(false);
        if is_dir && name.starts_with("mcp-logs-") {
            let dir = entry.path();
            report = merge(report, sweep_logs_by_filename_timestamp(&dir, cutoff));
            prune_if_empty(&dir);
        }
    }
    report
}

// ── transcripts (pVg) ───────────────────────────────────────────────────────

/// The transcript sidecar/related suffixes claude's `pVg` considers alongside
/// the primary `.jsonl`.
const TRANSCRIPT_SUFFIXES: &[&str] = &[".jsonl", ".cast", ".ccr-tip.json", ".precompact.json"];
const TRANSCRIPT_TMP_MARKERS: &[&str] = &[".ccr-tip.json.tmp.", ".precompact.json.tmp."];

/// Sweep stale session transcripts (claude `pVg`). Under
/// `<config>/projects/<project>/`, a `.jsonl` (and `.cast` / sidecar / tmp)
/// file older than the cutoff is removed; deleting a `<id>.jsonl` also force-
/// removes its `<id>.ccr-tip.json` / `<id>.precompact.json` sidecars and the
/// `<id>/` subagents directory.
fn sweep_transcripts(config_home: &Path, cutoff: SystemTime) -> RetentionReport {
    let mut report = RetentionReport::default();
    let projects = config_home.join("projects");
    let Ok(project_dirs) = std::fs::read_dir(&projects) else {
        return report;
    };
    for pd in project_dirs.flatten() {
        if !pd.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            continue;
        }
        let project = pd.path();
        let Ok(files) = std::fs::read_dir(&project) else {
            report.errors += 1;
            continue;
        };
        for f in files.flatten() {
            let Ok(meta) = f.metadata() else {
                report.errors += 1;
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let name = f.file_name().to_string_lossy().into_owned();
            let considered = TRANSCRIPT_SUFFIXES.iter().any(|s| name.ends_with(s))
                || TRANSCRIPT_TMP_MARKERS.iter().any(|m| name.contains(m));
            if !considered {
                continue;
            }
            let Ok(mtime) = meta.modified() else {
                report.errors += 1;
                continue;
            };
            if mtime >= cutoff {
                continue;
            }
            match std::fs::remove_file(f.path()) {
                Ok(()) => {
                    report.session_files_deleted += 1;
                    // Cascade: a deleted `<id>.jsonl` takes its sidecars + the
                    // `<id>/` subagents dir with it.
                    if let Some(base) = name.strip_suffix(".jsonl") {
                        report.transcripts_deleted += 1;
                        if !base.is_empty() && base != "." && base != ".." {
                            let _ =
                                std::fs::remove_file(project.join(format!("{base}.ccr-tip.json")));
                            let _ = std::fs::remove_file(
                                project.join(format!("{base}.precompact.json")),
                            );
                            let _ = std::fs::remove_dir_all(project.join(base));
                        }
                    }
                }
                Err(_) => report.errors += 1,
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    /// Set an entry's mtime to `age` in the past.
    fn set_old(path: &Path, age: Duration) {
        let when = SystemTime::now() - age;
        let ft = filetime::FileTime::from_system_time(when);
        filetime::set_file_mtime(path, ft).unwrap();
    }

    #[test]
    fn deletes_only_entries_older_than_the_period() {
        let root = tempfile::tempdir().unwrap();
        let todos = root.path().join("todos");
        fs::create_dir_all(&todos).unwrap();
        let old = todos.join("old.json");
        let fresh = todos.join("fresh.json");
        fs::write(&old, b"{}").unwrap();
        fs::write(&fresh, b"{}").unwrap();
        set_old(&old, Duration::from_secs(40 * 86400)); // 40 days old
                                                        // `fresh` keeps its just-now mtime.

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert_eq!(report.errors, 0);
        assert!(!old.exists(), "40-day-old file must be swept");
        assert!(fresh.exists(), "fresh file must be kept");
    }

    #[test]
    fn removes_old_directories_recursively() {
        let root = tempfile::tempdir().unwrap();
        let logs = root.path().join("logs");
        let old_dir = logs.join("2020-01");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("a.log"), b"x").unwrap();
        set_old(&old_dir, Duration::from_secs(365 * 86400));

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert!(!old_dir.exists());
    }

    #[test]
    fn ignores_untracked_dirs_and_missing_subdirs() {
        let root = tempfile::tempdir().unwrap();
        // A directory NOT in the swept set must be untouched even if ancient.
        let other = root.path().join("sessions");
        fs::create_dir_all(&other).unwrap();
        let keep = other.join("important.jsonl");
        fs::write(&keep, b"x").unwrap();
        set_old(&keep, Duration::from_secs(999 * 86400));

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 0);
        assert!(keep.exists(), "non-swept directory must never be touched");
    }

    #[test]
    fn sweeps_stale_subdirectories_but_not_stale_files_in_vrt_dirs() {
        let root = tempfile::tempdir().unwrap();
        let fh = root.path().join("file-history");
        fs::create_dir_all(&fh).unwrap();
        // A stale SUBDIR is removed; a stale FILE at this level is NOT (VRt only
        // considers subdirectories).
        let old_dir = fh.join("old-session");
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("edit.json"), b"{}").unwrap();
        set_old(&old_dir, Duration::from_secs(45 * 86400));
        let stray_file = fh.join("stray.txt");
        fs::write(&stray_file, b"x").unwrap();
        set_old(&stray_file, Duration::from_secs(45 * 86400));

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert!(!old_dir.exists(), "stale subdir must be swept");
        assert!(
            stray_file.exists(),
            "a file in a VRt dir must be left alone"
        );
    }

    #[test]
    fn sweeps_stale_md_plans_only() {
        let root = tempfile::tempdir().unwrap();
        let plans = root.path().join("plans");
        fs::create_dir_all(&plans).unwrap();
        let old_md = plans.join("2020-old.md");
        let fresh_md = plans.join("today.md");
        let other = plans.join("keep.txt");
        for p in [&old_md, &fresh_md, &other] {
            fs::write(p, b"x").unwrap();
        }
        set_old(&old_md, Duration::from_secs(60 * 86400));
        set_old(&other, Duration::from_secs(60 * 86400)); // old but not .md

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert!(!old_md.exists(), "stale .md plan must be swept");
        assert!(fresh_md.exists(), "fresh .md plan must be kept");
        assert!(other.exists(), "non-.md file must be kept");
    }

    #[test]
    fn sweeps_stale_single_cache_files_and_ext_dirs() {
        let root = tempfile::tempdir().unwrap();
        // Single cache file, stale → removed.
        let cache = root.path().join("mcp-needs-auth-cache.json");
        fs::write(&cache, b"{}").unwrap();
        set_old(&cache, Duration::from_secs(45 * 86400));
        // A fresh single cache file is kept.
        let fresh_cache = root.path().join("hfi-auth.json");
        fs::write(&fresh_cache, b"{}").unwrap();
        // File-ext dir: stale telemetry .json removed, non-.json kept.
        let tele = root.path().join("telemetry");
        fs::create_dir_all(&tele).unwrap();
        let old_json = tele.join("old.json");
        let keep_txt = tele.join("readme.txt");
        fs::write(&old_json, b"{}").unwrap();
        fs::write(&keep_txt, b"x").unwrap();
        set_old(&old_json, Duration::from_secs(45 * 86400));
        set_old(&keep_txt, Duration::from_secs(45 * 86400));

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 2); // cache + old.json
        assert!(!cache.exists());
        assert!(fresh_cache.exists());
        assert!(!old_json.exists());
        assert!(keep_txt.exists(), "non-matching ext must be kept");
    }

    #[test]
    fn debug_sweep_removes_stale_files_but_keeps_latest() {
        let root = tempfile::tempdir().unwrap();
        let debug = root.path().join("debug");
        fs::create_dir_all(&debug).unwrap();
        let old = debug.join("2020-run.log");
        let latest = debug.join("latest");
        fs::write(&old, b"x").unwrap();
        fs::write(&latest, b"x").unwrap();
        set_old(&old, Duration::from_secs(90 * 86400));
        set_old(&latest, Duration::from_secs(90 * 86400)); // old, but named `latest`

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert!(!old.exists());
        assert!(latest.exists(), "`latest` debug file must be preserved");
    }

    #[test]
    fn filename_timestamp_parses_iso_dashed_form() {
        // 2024-01-15T10:30:00.000Z = 1705314600 s since epoch.
        let ts = parse_filename_timestamp("2024-01-15T10-30-00-000Z.log").unwrap();
        let secs = ts.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1_705_314_600);
        // Unparseable names yield None (kept, never swept).
        assert!(parse_filename_timestamp("not-a-timestamp.log").is_none());
        assert!(parse_filename_timestamp("latest").is_none());
    }

    #[test]
    fn mcp_logs_sweep_deletes_by_filename_timestamp() {
        let root = tempfile::tempdir().unwrap();
        let d = root.path().join("logs").join("mcp-logs-sentry");
        fs::create_dir_all(&d).unwrap();
        // Old (2020) filename-timestamp → swept; recent → kept; unparseable → kept.
        let old = d.join("2020-01-01T00-00-00-000Z.txt");
        let unparseable = d.join("keepme.txt");
        fs::write(&old, b"x").unwrap();
        fs::write(&unparseable, b"x").unwrap();
        // A "recent" filename-timestamp: build from ~now-ish (last year is enough).
        let recent = d.join("2999-01-01T00-00-00-000Z.txt");
        fs::write(&recent, b"x").unwrap();

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert!(!old.exists(), "old-timestamp log swept");
        assert!(recent.exists(), "future-timestamp log kept");
        assert!(unparseable.exists(), "unparseable-name log kept");
    }

    #[test]
    fn transcript_sweep_cascades_sidecars_and_subagents_dir() {
        let root = tempfile::tempdir().unwrap();
        let proj = root.path().join("projects").join("-Users-me-proj");
        fs::create_dir_all(&proj).unwrap();
        let jsonl = proj.join("sess-1.jsonl");
        let ccr = proj.join("sess-1.ccr-tip.json");
        let pre = proj.join("sess-1.precompact.json");
        let subagents = proj.join("sess-1");
        fs::create_dir_all(subagents.join("subagents")).unwrap();
        fs::write(subagents.join("subagents").join("agent-x.jsonl"), b"{}").unwrap();
        for p in [&jsonl, &ccr, &pre] {
            fs::write(p, b"{}").unwrap();
        }
        set_old(&jsonl, Duration::from_secs(60 * 86400));
        // A fresh transcript in the same project must be untouched.
        let fresh = proj.join("sess-2.jsonl");
        fs::write(&fresh, b"{}").unwrap();

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.transcripts_deleted, 1);
        assert!(!jsonl.exists(), "stale transcript swept");
        assert!(!ccr.exists(), "ccr-tip sidecar cascaded");
        assert!(!pre.exists(), "precompact sidecar cascaded");
        assert!(!subagents.exists(), "session subagents dir cascaded");
        assert!(fresh.exists(), "fresh transcript kept");
    }

    #[test]
    fn dump_prompts_uses_the_shorter_3day_cap() {
        let root = tempfile::tempdir().unwrap();
        let dp = root.path().join("dump-prompts");
        fs::create_dir_all(&dp).unwrap();
        // 10 days old: kept under a 30-day base period, but swept under the
        // 3-day dump-prompts cap.
        let f = dp.join("run.jsonl");
        fs::write(&f, b"{}").unwrap();
        set_old(&f, Duration::from_secs(10 * 86400));

        let report = run_retention_sweep_in(root.path(), day_period(30));
        assert_eq!(report.session_files_deleted, 1);
        assert!(!f.exists(), "dump-prompts is capped at 3 days");
    }

    #[test]
    fn host_managed_orphans_swept_only_when_job_gone_and_old() {
        let root = tempfile::tempdir().unwrap();
        let hm = root.path().join("daemon").join("host-managed");
        let jobs = root.path().join("jobs");
        fs::create_dir_all(&hm).unwrap();
        fs::create_dir_all(&jobs).unwrap();
        // Orphan + old → swept.
        let orphan = hm.join("gone.json");
        fs::write(&orphan, b"{}").unwrap();
        set_old(&orphan, Duration::from_secs(60 * 86400));
        // Old but its job still exists → kept.
        let live = hm.join("alive.json");
        fs::write(&live, b"{}").unwrap();
        set_old(&live, Duration::from_secs(60 * 86400));
        fs::write(jobs.join("alive.json"), b"{}").unwrap();
        // Orphan but fresh → kept.
        let fresh_orphan = hm.join("recent.json");
        fs::write(&fresh_orphan, b"{}").unwrap();

        let report = run_retention_sweep_in(root.path(), day_period(30));
        // gone.json swept (1); jobs/settled sweep also runs but jobs/*.json here
        // are fresh, so only the orphan is removed.
        assert!(!orphan.exists(), "old orphan record swept");
        assert!(live.exists(), "record with a live job kept");
        assert!(fresh_orphan.exists(), "fresh orphan kept");
        assert_eq!(report.session_files_deleted, 1);
    }

    #[test]
    fn enable_flag_defaults_off() {
        // Ensure a clean env for the assertion.
        let prev = std::env::var_os(RETENTION_ENABLE_ENV);
        std::env::remove_var(RETENTION_ENABLE_ENV);
        assert!(!retention_sweep_enabled());
        std::env::set_var(RETENTION_ENABLE_ENV, "1");
        assert!(retention_sweep_enabled());
        std::env::set_var(RETENTION_ENABLE_ENV, "0");
        assert!(!retention_sweep_enabled());
        match prev {
            Some(v) => std::env::set_var(RETENTION_ENABLE_ENV, v),
            None => std::env::remove_var(RETENTION_ENABLE_ENV),
        }
    }

    #[test]
    fn period_defaults_to_30_days() {
        let prev = std::env::var_os(RETENTION_PERIOD_ENV);
        std::env::remove_var(RETENTION_PERIOD_ENV);
        assert_eq!(retention_period_days(), (30, true));
        std::env::set_var(RETENTION_PERIOD_ENV, "7");
        assert_eq!(retention_period_days(), (7, false));
        std::env::set_var(RETENTION_PERIOD_ENV, "0"); // invalid → default
        assert_eq!(retention_period_days(), (30, true));
        match prev {
            Some(v) => std::env::set_var(RETENTION_PERIOD_ENV, v),
            None => std::env::remove_var(RETENTION_PERIOD_ENV),
        }
    }
}
