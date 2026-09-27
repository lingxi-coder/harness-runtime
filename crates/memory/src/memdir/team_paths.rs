//! Team-mem path resolution, gated on `settings.team_memory.enabled`.

use std::path::{Path, PathBuf};

/// Resolve `~/.lingxi/team-mem/` when explicitly enabled.
///
/// Returns `None` when `enabled == false`. Auto-detection from
/// filesystem presence is intentionally NOT used — caller must opt in
/// via the `settings.team_memory.enabled` bool.
#[must_use]
pub fn resolve_team_memory_dir(home: &Path, enabled: bool) -> Option<PathBuf> {
    if !enabled {
        return None;
    }
    Some(
        home.join(branding::DOT_DIR)
            .join(super::paths::TEAM_MEM_SUBDIR),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_returns_none_even_if_dir_exists() {
        // Auto-detect is NOT used; the bool is the only signal.
        let tmp = tempfile::TempDir::new().unwrap();
        let home = tmp.path();
        // Pretend the dir exists on disk anyway:
        std::fs::create_dir_all(home.join(".lingxi").join("team-mem")).unwrap();
        assert_eq!(resolve_team_memory_dir(home, false), None);
    }

    #[test]
    fn enabled_returns_dot_claude_team_mem() {
        let home = std::path::PathBuf::from("/home/u");
        let p = resolve_team_memory_dir(&home, true).expect("Some");
        assert_eq!(p, std::path::PathBuf::from("/home/u/.lingxi/team-mem"));
    }
}
