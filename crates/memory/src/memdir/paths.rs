//! Config-home memdir resolution: `<config-home>/{memdir, agents/session-memory,
//! team-mem}`, where config-home honors `$LINGXI_CONFIG_DIR` (else `~/.claude`).

use std::path::{Path, PathBuf};

/// Subdirectory under the config-home for individual memdir entries.
pub const MEMDIR_SUBDIR: &str = "memdir";
/// Subdirectory holding one folder per project, mirroring where sessions live.
pub const PROJECTS_SUBDIR: &str = "projects";

/// `<config-home>/projects/<project-dir>/memdir` — the User-tier memdir for the
/// project rooted at `cwd`.
///
/// claude-code scopes auto-memory PER PROJECT
/// (`<base>/projects/<sanitized-root>/memory`, oracle `defaultPath()` in
/// `src_166572870.js`). The port used a single `<config-home>/memdir` for every
/// repository, so a memory written while working on one codebase was recalled
/// while working on another — the memories are project-specific advice, so
/// cross-repo bleed is the user-visible symptom.
///
/// The project folder name is [`session::jsonl::path::project_dir_name`], the
/// SAME sanitizer the session transcripts already use, so a project's memories
/// sit beside its sessions instead of inventing a second naming scheme.
#[must_use]
pub fn user_memdir_for_project(config_home: &Path, cwd: &Path) -> PathBuf {
    config_home
        .join(PROJECTS_SUBDIR)
        .join(session::jsonl::path::project_dir_name(
            &cwd.to_string_lossy(),
        ))
        .join(MEMDIR_SUBDIR)
}
/// Subdirectory under the config-home for team-shared entries.
pub const TEAM_MEM_SUBDIR: &str = "team-mem";
/// Subdirectory under the config-home holding agent artifacts (session memory).
pub const AGENTS_SUBDIR: &str = "agents";
/// Subdirectory under `agents/` holding per-session memory files (`<id>.md`) —
/// 1:1 with the `session_memory` WRITE path and `detect_session_file_type`.
pub const SESSION_MEMORY_SUBDIR: &str = "session-memory";

/// Resolved roots for the memdir scan.
#[derive(Debug, Clone)]
pub struct MemdirRoots {
    /// `<config-home>/memdir/` (always set; may not exist on disk).
    pub user_memdir: PathBuf,
    /// `<config-home>/agents/session-memory/` — the Session tier, holding the
    /// per-session memory files `session_memory` writes. Always set; the dir may
    /// not exist until a session-memory extraction has run.
    pub session_memdir: PathBuf,
    /// `<config-home>/team-mem/` — `None` when `team_memory.enabled == false`.
    pub team_memdir: Option<PathBuf>,
}

/// Resolve memdir roots under `home`, honoring `$LINGXI_CONFIG_DIR` for the
/// config-home (matching the `session_memory` WRITE path, so the Session-tier
/// scan reads exactly what writes produce). Team root is `Some` only when
/// `team_enabled == true` (the bool comes from `settings.team_memory.enabled`;
/// auto-detection from filesystem presence is intentionally NOT used).
#[must_use]
pub fn memdir_path(home: &Path, cwd: &Path, team_enabled: bool) -> MemdirRoots {
    memdir_roots_at(&crate::lingxi_md::user_config_dir(home), cwd, team_enabled)
}

/// Pure roots resolver from an already-resolved `config_home` (the `.claude`
/// dir). Env-free, so unit tests are deterministic; [`memdir_path`] is the thin
/// `$LINGXI_CONFIG_DIR`-honoring wrapper.
#[must_use]
pub fn memdir_roots_at(config_home: &Path, cwd: &Path, team_enabled: bool) -> MemdirRoots {
    MemdirRoots {
        user_memdir: user_memdir_for_project(config_home, cwd),
        session_memdir: config_home.join(AGENTS_SUBDIR).join(SESSION_MEMORY_SUBDIR),
        team_memdir: team_enabled.then(|| config_home.join(TEAM_MEM_SUBDIR)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn roots_are_derived_under_config_home() {
        let cfg = PathBuf::from("/home/u/.lingxi");
        let roots = memdir_roots_at(&cfg, Path::new("/work/repo"), false);
        assert_eq!(
            roots.user_memdir,
            PathBuf::from("/home/u/.lingxi/projects/-work-repo/memdir")
        );
        assert_eq!(
            roots.session_memdir,
            PathBuf::from("/home/u/.lingxi/agents/session-memory")
        );
        assert_eq!(roots.team_memdir, None);
    }

    #[test]
    fn team_memdir_is_team_mem_when_enabled() {
        let cfg = PathBuf::from("/home/u/.lingxi");
        let roots = memdir_roots_at(&cfg, Path::new("/work/repo"), true);
        assert_eq!(
            roots.team_memdir,
            Some(PathBuf::from("/home/u/.lingxi/team-mem"))
        );
    }

    /// MEM-2 — the whole point: two repositories must not share a memdir.
    /// Before this the User tier was a single `<config-home>/memdir` for every
    /// project, so advice written about one codebase surfaced while working on
    /// another.
    #[test]
    fn two_projects_get_different_user_memdirs() {
        let cfg = PathBuf::from("/home/u/.lingxi");
        let a = memdir_roots_at(&cfg, Path::new("/work/alpha"), false).user_memdir;
        let b = memdir_roots_at(&cfg, Path::new("/work/beta"), false).user_memdir;
        assert_ne!(a, b, "different projects must not share a memdir");
        assert!(a.starts_with(cfg.join("projects")));
        assert!(b.starts_with(cfg.join("projects")));
    }

    /// The project folder is the SAME one the session transcripts use, so a
    /// project's memories land beside its sessions. Pinning this stops the two
    /// from drifting into two naming schemes.
    #[test]
    fn the_project_folder_matches_the_session_transcript_folder() {
        let cwd = "/Users/x/Projects/Thing";
        let roots = memdir_roots_at(Path::new("/cfg"), Path::new(cwd), false);
        let expected = session::jsonl::path::project_dir_name(cwd);
        assert_eq!(
            roots.user_memdir,
            PathBuf::from("/cfg")
                .join("projects")
                .join(&expected)
                .join("memdir")
        );
    }

    #[test]
    fn subdir_constants_match_spec_wire_identifiers() {
        assert_eq!(MEMDIR_SUBDIR, "memdir");
        assert_eq!(TEAM_MEM_SUBDIR, "team-mem");
        assert_eq!(AGENTS_SUBDIR, "agents");
        assert_eq!(SESSION_MEMORY_SUBDIR, "session-memory");
    }
}
