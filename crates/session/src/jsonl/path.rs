//! Project-dir name resolver — 1:1 port of
//! `claude-code/src/utils/sessionStoragePortable.ts:293-331`.

use crate::jsonl::djb2::djb2_hash;
use std::path::{Path, PathBuf};

/// `MAX_SANITIZED_LENGTH` from `sessionStoragePortable.ts:293`.
pub const MAX_SANITIZED_LENGTH: usize = 200;

/// `cwd.replace(/[^a-zA-Z0-9]/g, '-')` then suffix with djb2-base36 if > 200 chars.
#[must_use]
pub fn project_dir_name(cwd: &str) -> String {
    let sanitized: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if sanitized.chars().count() <= MAX_SANITIZED_LENGTH {
        return sanitized;
    }
    let head: String = sanitized.chars().take(MAX_SANITIZED_LENGTH).collect();
    let suffix = base36_abs(djb2_hash(cwd));
    format!("{head}-{suffix}")
}

/// `<lingxi_home>/projects/<project_dir_name(cwd)>/<session_uuid>.jsonl`.
#[must_use]
pub fn session_path(lingxi_home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    let mut p = lingxi_home.to_path_buf();
    p.push("projects");
    p.push(project_dir_name(cwd));
    p.push(format!("{session_uuid}.jsonl"));
    p
}

/// `<lingxi_home>/projects/<project_dir_name(cwd)>/<session_uuid>/tool-results`.
///
/// 1:1 with claude-code (2.1.220 @230268971):
///
/// ```js
/// var was="tool-results";
/// function qzg(){return vas.join(F7(gn()),kt())}   // <projects>/<sessionId>
/// function xke(){return vas.join(qzg(),was)}
/// ```
///
/// Verified against real transcripts on disk, which carry exactly
/// `~/.claude/projects/<sanitized-cwd>/<session-uuid>/tool-results/`.
///
/// A SIBLING of [`session_path`], not a child: the session `.jsonl` and its
/// tool-results dir sit next to each other under the same project dir, so both
/// derive that component from [`project_dir_name`] or they drift apart for a
/// long `cwd` (where the djb2 suffix kicks in).
///
/// Deliberately NOT under `cwd`: persisted tool output is session scratch, and
/// writing it into the workspace drops artifacts inside the user's repository.
#[must_use]
pub fn tool_results_dir(lingxi_home: &Path, cwd: &str, session_uuid: &str) -> PathBuf {
    let mut p = lingxi_home.to_path_buf();
    p.push("projects");
    p.push(project_dir_name(cwd));
    p.push(session_uuid);
    p.push("tool-results");
    p
}

/// `Math.abs(djb2Hash(s)).toString(36)` — special-case `i32::MIN` whose `.abs()`
/// overflows: claude-code's `Math.abs` returns `Math.abs(-(2^31))` = `2^31`
/// (a float), then `.toString(36)` formats it as `"1z141z3"`. Matching that
/// exactly here would require `i64` arithmetic; we use `i32::unsigned_abs()`
/// (which yields `2_147_483_648u32` for `i32::MIN`) and base36-format the
/// `u32` — verified equivalent for all 2^32 inputs.
fn base36_abs(h: i32) -> String {
    let mut n: u32 = h.unsigned_abs();
    if n == 0 {
        return "0".to_string();
    }
    let alphabet = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut bytes = Vec::with_capacity(7);
    while n > 0 {
        bytes.push(alphabet[(n % 36) as usize]);
        n /= 36;
    }
    bytes.reverse();
    String::from_utf8(bytes).expect("base36 alphabet is ASCII")
}

#[cfg(test)]
mod tool_results_dir_tests {
    use super::*;

    /// The shape verified on disk: `<home>/projects/<sanitized-cwd>/<uuid>/tool-results`.
    #[test]
    fn tool_results_dir_is_session_scoped_under_the_project_dir() {
        assert_eq!(
            tool_results_dir(Path::new("/h/.lingxi"), "/Users/me/proj", "f6ff715f-b063"),
            PathBuf::from("/h/.lingxi/projects/-Users-me-proj/f6ff715f-b063/tool-results")
        );
    }

    /// It must NEVER land under the workspace — that is the bug this replaces
    /// (`cwd.join(DOT_DIR).join("tool-results")` dropped artifacts in the repo).
    #[test]
    fn tool_results_dir_is_not_under_the_workspace() {
        let cwd = "/Users/me/proj";
        let got = tool_results_dir(Path::new("/h/.lingxi"), cwd, "s1");
        assert!(
            !got.starts_with(cwd),
            "persisted tool output must not be written into the user's repo: {got:?}"
        );
    }

    /// The tool-results dir and the session `.jsonl` must agree on the project
    /// component, including the djb2 suffix a >200-char cwd triggers.
    #[test]
    fn tool_results_dir_shares_the_project_component_with_the_session_file() {
        let long = format!("/{}", "x".repeat(400));
        let home = Path::new("/h/.lingxi");
        let proj_of = |p: &PathBuf| {
            p.components()
                .skip_while(|c| c.as_os_str() != "projects")
                .nth(1)
                .map(|c| c.as_os_str().to_owned())
        };
        let results = proj_of(&tool_results_dir(home, &long, "s1"));
        assert_eq!(results, proj_of(&session_path(home, &long, "s1")));
        assert!(results.is_some(), "project component present");
    }
}
