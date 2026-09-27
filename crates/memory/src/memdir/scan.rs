//! Memdir enumeration + 365-day hard drop.

use crate::{
    parse_markdown_with_frontmatter, MemoryFrontmatter, MAX_MEMORY_FILE_SIZE,
    MEMORY_AGE_HARD_DROP_DAYS,
};
use protocol::{MemoryEntry, MemoryEntryTier};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::time::SystemTime;

const MAX_MEMDIR_ENTRIES: usize = 256;
const MAX_MEMDIR_TOTAL_BYTES: u64 = 50 * 1024 * 1024;

#[derive(Clone, Copy)]
struct ScanLimits {
    max_entries: usize,
    max_total_bytes: u64,
    max_candidates_per_tier: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_MEMDIR_ENTRIES,
            max_total_bytes: MAX_MEMDIR_TOTAL_BYTES,
            max_candidates_per_tier: MAX_MEMDIR_ENTRIES * 8,
        }
    }
}

#[derive(Default)]
struct ScanBudget {
    entries: usize,
    total_bytes: u64,
}

struct ScanCandidate {
    path: std::path::PathBuf,
    tier: MemoryEntryTier,
    age_days: u64,
    size_bytes: u64,
}

struct TierScan {
    tier: MemoryEntryTier,
    reserved_entries: usize,
    reserved_bytes: u64,
    selected_entries: usize,
    selected_bytes: u64,
    pending: Vec<ScanCandidate>,
    selected: Vec<ScanCandidate>,
}

/// Snapshot of memdir scan result.
#[derive(Debug, Default)]
pub struct MemdirSnapshot {
    /// Entries surviving the 365-day hygiene drop. Order is insertion
    /// order (user dir then session dir then team dir, lexicographic within
    /// each).
    pub entries: Vec<MemoryEntry>,
    /// Frontmatter parsed from the same redacted bytes as `entries`. The
    /// public `MemoryEntry` body intentionally remains frontmatter-free, while
    /// the selector still needs descriptions/tags to rank candidates.
    pub(crate) frontmatter: HashMap<std::path::PathBuf, MemoryFrontmatter>,
}

/// Scan with `now = SystemTime::now()`. Convenience wrapper around
/// [`scan_memdir_at`].
///
/// # Errors
///
/// Returns `std::io::Error` only for directory-iteration errors that
/// aren't `NotFound` (missing dirs are silently treated as empty).
pub fn scan_memdir(roots: &super::paths::MemdirRoots) -> std::io::Result<MemdirSnapshot> {
    scan_memdir_at(roots, SystemTime::now())
}

/// Scan with explicit `now` (testable). Files older than
/// `MEMORY_AGE_HARD_DROP_DAYS` are dropped; oversized files (>10 MB) are
/// dropped too (their event is emitted by the caller through
/// [`crate::lingxi_md::loader::emit_file_too_large`] for hierarchy files;
/// memdir files emit the same event via the loader path used by the
/// engine wrapper — see Task 13).
///
/// # Errors
///
/// Returns `std::io::Error` for directory-iteration errors other than
/// `NotFound`.
pub fn scan_memdir_at(
    roots: &super::paths::MemdirRoots,
    now: SystemTime,
) -> std::io::Result<MemdirSnapshot> {
    scan_memdir_at_with_limits(roots, now, ScanLimits::default())
}

fn scan_memdir_at_with_limits(
    roots: &super::paths::MemdirRoots,
    now: SystemTime,
    limits: ScanLimits,
) -> std::io::Result<MemdirSnapshot> {
    let mut scans = Vec::with_capacity(3);
    scans.push(TierScan {
        tier: MemoryEntryTier::User,
        reserved_entries: 0,
        reserved_bytes: 0,
        selected_entries: 0,
        selected_bytes: 0,
        pending: enumerate(
            &roots.user_memdir,
            MemoryEntryTier::User,
            now,
            limits.max_candidates_per_tier,
        )?,
        selected: Vec::new(),
    });
    // Session tier — per-session memory files written by `session_memory`. The
    // dir is silently absent until an extraction has run (NotFound is ignored).
    scans.push(TierScan {
        tier: MemoryEntryTier::Session,
        reserved_entries: 0,
        reserved_bytes: 0,
        selected_entries: 0,
        selected_bytes: 0,
        pending: enumerate(
            &roots.session_memdir,
            MemoryEntryTier::Session,
            now,
            limits.max_candidates_per_tier,
        )?,
        selected: Vec::new(),
    });
    if let Some(team) = roots.team_memdir.as_deref() {
        scans.push(TierScan {
            tier: MemoryEntryTier::Team,
            reserved_entries: 0,
            reserved_bytes: 0,
            selected_entries: 0,
            selected_bytes: 0,
            pending: enumerate(
                team,
                MemoryEntryTier::Team,
                now,
                limits.max_candidates_per_tier,
            )?,
            selected: Vec::new(),
        });
    }

    assign_reserved_budget(&mut scans, limits);

    let mut budget = ScanBudget::default();
    for tier in [
        MemoryEntryTier::Session,
        MemoryEntryTier::Team,
        MemoryEntryTier::User,
    ] {
        if let Some(scan) = scans.iter_mut().find(|scan| scan.tier == tier) {
            select_entries(scan, limits, &mut budget, true);
        }
    }
    for tier in [
        MemoryEntryTier::Session,
        MemoryEntryTier::Team,
        MemoryEntryTier::User,
    ] {
        if let Some(scan) = scans.iter_mut().find(|scan| scan.tier == tier) {
            select_entries(scan, limits, &mut budget, false);
        }
    }

    let mut entries = Vec::new();
    let mut frontmatter = HashMap::new();
    for tier in [
        MemoryEntryTier::User,
        MemoryEntryTier::Session,
        MemoryEntryTier::Team,
    ] {
        if let Some(scan) = scans.iter_mut().find(|scan| scan.tier == tier) {
            for candidate in scan.selected.drain(..) {
                let (body, parsed_frontmatter) = std::fs::read_to_string(&candidate.path)
                    .map(|raw| sanitize_body(&raw))
                    .unwrap_or_default();
                if let Some(metadata) = parsed_frontmatter {
                    frontmatter.insert(candidate.path.clone(), metadata);
                }
                entries.push(MemoryEntry {
                    path: candidate.path,
                    tier: candidate.tier,
                    body,
                    age_days: candidate.age_days,
                    size_bytes: candidate.size_bytes,
                });
            }
        }
    }
    Ok(MemdirSnapshot {
        entries,
        frontmatter,
    })
}

fn enumerate(
    dir: &Path,
    tier: MemoryEntryTier,
    now: SystemTime,
    max_candidates_per_tier: usize,
) -> std::io::Result<Vec<ScanCandidate>> {
    if max_candidates_per_tier == 0 {
        return Ok(Vec::new());
    }
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        // Missing dirs are silently treated as empty. ENOTDIR (errno 20
        // on unix) covers cases like the sentinel `/dev/null` placeholder
        // used by `team_prompts::collect_team_prompts` when no user_memdir
        // is needed. We match by raw OS error rather than
        // `ErrorKind::NotADirectory` (unstable on stable Rust as of 1.83).
        Err(e) if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
            return Ok(Vec::new())
        }
        Err(e) => return Err(e),
    };

    // Keep only the lexicographically earliest paths while streaming
    // `read_dir`, so metadata work and retained candidates stay bounded even
    // for huge memdirs.
    let mut retained: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    for entry in read.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if retained.len() >= max_candidates_per_tier {
            if let Some(largest) = retained.iter().next_back() {
                if path.as_path() >= largest.as_path() {
                    continue;
                }
            }
        }
        retained.insert(path);
        if retained.len() > max_candidates_per_tier {
            retained.pop_last();
        }
    }

    let mut out = Vec::with_capacity(retained.len());
    for path in retained {
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        // On 32-bit targets a >4 GB file overflows `usize`; in that case it
        // is by definition over the 10 MB cap, so treat the conversion failure
        // as "too large" rather than rejecting it as an I/O error.
        let over_cap = match usize::try_from(meta.len()) {
            Ok(n) => n > MAX_MEMORY_FILE_SIZE,
            Err(_) => true,
        };
        if over_cap {
            // Oversized — skip (caller emits tengu_memory_file_too_large
            // via the loader wrapper in Task 13).
            continue;
        }
        let mtime = meta.modified().unwrap_or(now);
        let age_days = now
            .duration_since(mtime)
            .map(|d| d.as_secs() / 86_400)
            .unwrap_or(0);
        if age_days > MEMORY_AGE_HARD_DROP_DAYS {
            continue;
        }
        out.push(ScanCandidate {
            path,
            tier,
            age_days,
            size_bytes: meta.len(),
        });
    }
    Ok(out)
}

fn assign_reserved_budget(scans: &mut [TierScan], limits: ScanLimits) {
    let active: Vec<_> = scans
        .iter()
        .enumerate()
        .filter_map(|(idx, scan)| (!scan.pending.is_empty()).then_some(idx))
        .collect();
    if active.is_empty() {
        return;
    }

    let active_count = active.len();
    let base_entries = limits.max_entries / active_count;
    let extra_entries = limits.max_entries % active_count;
    let base_bytes = limits.max_total_bytes / active_count as u64;
    let extra_bytes = limits.max_total_bytes % active_count as u64;

    let mut priority = active;
    priority.sort_by_key(|&idx| tier_priority(scans[idx].tier));
    for (slot, idx) in priority.into_iter().enumerate() {
        scans[idx].reserved_entries = base_entries + usize::from(slot < extra_entries);
        scans[idx].reserved_bytes = base_bytes + u64::from((slot as u64) < extra_bytes);
    }
}

fn select_entries(
    scan: &mut TierScan,
    limits: ScanLimits,
    budget: &mut ScanBudget,
    reserved_only: bool,
) {
    if budget.entries >= limits.max_entries || budget.total_bytes >= limits.max_total_bytes {
        return;
    }

    let mut deferred = Vec::new();
    for candidate in scan.pending.drain(..) {
        if budget.entries >= limits.max_entries || budget.total_bytes >= limits.max_total_bytes {
            deferred.push(candidate);
            continue;
        }
        if reserved_only && scan.selected_entries >= scan.reserved_entries {
            deferred.push(candidate);
            continue;
        }
        if reserved_only
            && scan.selected_bytes.saturating_add(candidate.size_bytes) > scan.reserved_bytes
        {
            deferred.push(candidate);
            continue;
        }
        if budget.total_bytes.saturating_add(candidate.size_bytes) > limits.max_total_bytes {
            deferred.push(candidate);
            continue;
        }

        scan.selected_entries += 1;
        scan.selected_bytes = scan.selected_bytes.saturating_add(candidate.size_bytes);
        budget.entries += 1;
        budget.total_bytes = budget.total_bytes.saturating_add(candidate.size_bytes);
        scan.selected.push(candidate);
    }
    scan.pending = deferred;
}

/// Which tier gets the REMAINDER of a divided byte/entry budget first
/// (lowest number = first slot).
///
/// This is an anti-starvation order, NOT a relevance order, and it deliberately
/// disagrees with [`super::find::tier_weight_bps`]: `Project` ranks LAST here
/// and SECOND there. A project memdir is typically the largest, so giving it
/// the leftovers last is what keeps a handful of session and team files from
/// being crowded out — see `scan_preserves_room_for_session_and_team_tiers`.
///
/// The two orderings answer different questions. Do not "reconcile" them;
/// `the_two_tier_orderings_disagree_on_purpose` pins the divergence.
const fn tier_priority(tier: MemoryEntryTier) -> u8 {
    match tier {
        MemoryEntryTier::Session => 0,
        MemoryEntryTier::Team => 1,
        MemoryEntryTier::User => 2,
        MemoryEntryTier::Project => 3,
    }
}

fn sanitize_body(raw: &str) -> (String, Option<MemoryFrontmatter>) {
    // Redact before parsing so secrets in descriptions/tags cannot reach the
    // selector side query. The public entry body remains the parsed markdown
    // body, while the parsed metadata is retained in the snapshot sidecar.
    let redacted = crate::secret_scan::redact(raw);
    match parse_markdown_with_frontmatter(&redacted) {
        Ok((metadata, body)) => (body, Some(metadata)),
        Err(_) => (redacted, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memdir::paths::{memdir_path, MemdirRoots};
    use crate::{secret_scan::redact, MEMORY_AGE_HARD_DROP_DAYS};
    use protocol::MemoryEntryTier;

    /// The budget order and the relevance order rank `Project` at opposite
    /// ends, and that is deliberate — see `tier_priority`'s docs. Pinned so a
    /// future reader who spots the "inconsistency" and reconciles the two
    /// breaks a test instead of quietly changing which memories survive a
    /// byte budget.
    #[test]
    fn the_two_tier_orderings_disagree_on_purpose() {
        use crate::memdir::find::tier_weight_bps;

        // Relevance: Project outranks Team and User.
        assert!(tier_weight_bps(MemoryEntryTier::Project) > tier_weight_bps(MemoryEntryTier::Team));
        assert!(tier_weight_bps(MemoryEntryTier::Team) > tier_weight_bps(MemoryEntryTier::User));

        // Leftover budget: Project comes LAST, behind both of them.
        assert!(tier_priority(MemoryEntryTier::Project) > tier_priority(MemoryEntryTier::Team));
        assert!(tier_priority(MemoryEntryTier::Project) > tier_priority(MemoryEntryTier::User));

        // Session leads both orderings — the one rung they agree on.
        assert_eq!(tier_priority(MemoryEntryTier::Session), 0);
        assert!(
            tier_weight_bps(MemoryEntryTier::Session) > tier_weight_bps(MemoryEntryTier::Project)
        );
    }
    use std::fs;
    use std::time::{Duration, SystemTime};
    use tempfile::TempDir;

    fn write_dated(path: &std::path::Path, bytes: &[u8], age_days: u64) {
        fs::write(path, bytes).unwrap();
        let mtime = SystemTime::now() - Duration::from_secs(age_days * 86_400);
        filetime::set_file_mtime(path, filetime::FileTime::from_system_time(mtime)).unwrap();
    }

    #[test]
    fn scan_drops_entries_older_than_365_days() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let memdir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        fs::create_dir_all(&memdir).unwrap();
        write_dated(&memdir.join("fresh.md"), b"fresh\n", 10);
        write_dated(
            &memdir.join("stale.md"),
            b"stale\n",
            MEMORY_AGE_HARD_DROP_DAYS + 1,
        );

        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at(&roots, SystemTime::now()).unwrap();
        let names: Vec<_> = snap
            .entries
            .iter()
            .filter_map(|e| {
                e.path
                    .file_name()
                    .and_then(|n| n.to_str().map(String::from))
            })
            .collect();
        assert!(names.contains(&"fresh.md".into()));
        assert!(!names.contains(&"stale.md".into()), "stale dropped at scan");
    }

    #[test]
    fn user_tier_assigned_for_user_memdir() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let memdir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        fs::create_dir_all(&memdir).unwrap();
        fs::write(memdir.join("u.md"), b"u\n").unwrap();
        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at(&roots, SystemTime::now()).unwrap();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].tier, MemoryEntryTier::User);
    }

    #[test]
    fn session_tier_assigned_for_session_memdir() {
        // A file under `<config-home>/agents/session-memory` (exactly where
        // `session_memory` writes) scans as the Session tier — the re-load half
        // of session-memory, connected via the shared config-home resolution.
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let session_dir = home.join(".lingxi").join("agents").join("session-memory");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("sess-1.md"), b"durable note\n").unwrap();
        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at(&roots, SystemTime::now()).unwrap();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(snap.entries[0].tier, MemoryEntryTier::Session);
    }

    #[test]
    fn team_tier_assigned_for_team_memdir() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let team = home.join(".lingxi").join("team-mem");
        fs::create_dir_all(&team).unwrap();
        fs::write(team.join("t.md"), b"t\n").unwrap();
        let roots = memdir_path(home, std::path::Path::new("/proj"), true);
        let snap = scan_memdir_at(&roots, SystemTime::now()).unwrap();
        assert!(snap.entries.iter().any(|e| e.tier == MemoryEntryTier::Team));
    }

    #[test]
    fn missing_dirs_yield_empty_snapshot() {
        let tmp = TempDir::new().unwrap();
        let roots = MemdirRoots {
            user_memdir: tmp.path().join("does/not/exist"),
            session_memdir: tmp.path().join("does/not/exist-sm"),
            team_memdir: None,
        };
        let snap = scan_memdir_at(&roots, SystemTime::now()).unwrap();
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn scan_strips_frontmatter_and_redacts_secrets() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let memdir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        fs::create_dir_all(&memdir).unwrap();
        let raw = concat!(
            "---\n",
            "description: shell tips\n",
            "---\n",
            "creds: AKIAIOSFODNN7EXAMPLE\n",
            "use fd not find\n"
        );
        fs::write(memdir.join("secret.md"), raw).unwrap();

        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at(&roots, SystemTime::now()).unwrap();
        assert_eq!(snap.entries.len(), 1);
        assert_eq!(
            snap.entries[0].body,
            redact("creds: AKIAIOSFODNN7EXAMPLE\nuse fd not find\n")
        );
        assert_eq!(
            snap.frontmatter
                .get(&snap.entries[0].path)
                .expect("frontmatter sidecar")
                .description,
            "shell tips"
        );
        assert!(
            !snap.entries[0].body.contains("description: shell tips"),
            "frontmatter must be stripped before surfacing"
        );
    }

    #[test]
    fn scan_caps_entries_and_total_bytes_in_deterministic_order() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let memdir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        fs::create_dir_all(&memdir).unwrap();
        fs::write(memdir.join("a.md"), "aaaa").unwrap();
        fs::write(memdir.join("b.md"), "bbbb").unwrap();
        fs::write(memdir.join("c.md"), "cccc").unwrap();

        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at_with_limits(
            &roots,
            SystemTime::now(),
            ScanLimits {
                max_entries: 2,
                max_total_bytes: 8,
                ..ScanLimits::default()
            },
        )
        .unwrap();
        let names: Vec<_> = snap
            .entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.md", "b.md"]);
        assert_eq!(
            snap.entries
                .iter()
                .map(|entry| entry.size_bytes)
                .sum::<u64>(),
            8
        );
    }

    #[test]
    fn scan_preserves_room_for_session_and_team_tiers() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let user_dir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        let session_dir = home.join(".lingxi").join("agents").join("session-memory");
        let team_dir = home.join(".lingxi").join("team-mem");
        fs::create_dir_all(&user_dir).unwrap();
        fs::create_dir_all(&session_dir).unwrap();
        fs::create_dir_all(&team_dir).unwrap();

        for name in ["a.md", "b.md", "c.md"] {
            fs::write(user_dir.join(name), "user").unwrap();
        }
        fs::write(session_dir.join("session.md"), "sess").unwrap();
        fs::write(team_dir.join("team.md"), "team").unwrap();

        let roots = memdir_path(home, std::path::Path::new("/proj"), true);
        let snap = scan_memdir_at_with_limits(
            &roots,
            SystemTime::now(),
            ScanLimits {
                max_entries: 3,
                max_total_bytes: 12,
                ..ScanLimits::default()
            },
        )
        .unwrap();

        assert_eq!(snap.entries.len(), 3);
        assert!(snap
            .entries
            .iter()
            .any(|entry| entry.tier == MemoryEntryTier::User));
        assert!(snap
            .entries
            .iter()
            .any(|entry| entry.tier == MemoryEntryTier::Session));
        assert!(snap
            .entries
            .iter()
            .any(|entry| entry.tier == MemoryEntryTier::Team));
    }

    #[test]
    fn scan_skips_large_candidate_and_keeps_smaller_later_file() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let memdir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        fs::create_dir_all(&memdir).unwrap();
        fs::write(memdir.join("a-large.md"), "123456").unwrap();
        fs::write(memdir.join("b-small.md"), "1234").unwrap();

        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at_with_limits(
            &roots,
            SystemTime::now(),
            ScanLimits {
                max_entries: 2,
                max_total_bytes: 4,
                ..ScanLimits::default()
            },
        )
        .unwrap();

        let names: Vec<_> = snap
            .entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["b-small.md"]);
    }

    #[test]
    fn scan_bounds_per_tier_candidates_deterministically() {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path();
        let memdir = home
            .join(".lingxi")
            .join("projects")
            .join("-proj")
            .join("memdir");
        fs::create_dir_all(&memdir).unwrap();

        for name in ["d.md", "b.md", "a.md", "f.md", "c.md", "e.md"] {
            fs::write(memdir.join(name), name).unwrap();
        }

        let roots = memdir_path(home, std::path::Path::new("/proj"), false);
        let snap = scan_memdir_at_with_limits(
            &roots,
            SystemTime::now(),
            ScanLimits {
                max_entries: 10,
                max_total_bytes: 1024,
                max_candidates_per_tier: 3,
            },
        )
        .unwrap();

        let names: Vec<_> = snap
            .entries
            .iter()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec!["a.md", "b.md", "c.md"]);
    }
}
