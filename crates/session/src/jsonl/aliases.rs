//! Direct session-directory links written by `/add-dir` (Claude Code 2.1.307).
//!
//! A session stored in project directory A is made discoverable from B by
//! appending A's complete project-directory path to B's `.session-aliases`.
//! Links are directional and are never followed recursively.

use super::path::project_dir_name;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use unicode_normalization::UnicodeNormalization;

/// Filename used by the native session-directory link format.
pub const SESSION_ALIASES_FILE: &str = ".session-aliases";

/// Read direct linked project directories in their first-seen order.
///
/// Native `W0e` splits only on LF, drops only empty strings, and deduplicates
/// exact strings. In particular, CR and spaces are part of a path, not trim
/// characters. Missing or unreadable link files behave as an empty list.
pub async fn read_session_aliases(project_dir: &Path) -> Vec<PathBuf> {
    let Ok(content) = tokio::fs::read_to_string(project_dir.join(SESSION_ALIASES_FILE)).await
    else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    content
        .split('\n')
        .filter(|line| !line.is_empty() && seen.insert(*line))
        .map(PathBuf::from)
        .collect()
}

/// Native resume's wS normalizes both resolved and retained paths to NFC.
pub(crate) fn normalized_directory_path(path: &Path) -> PathBuf {
    let resolved = match std::fs::canonicalize(path) {
        Ok(resolved) => resolved,
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(path = %path.display(), %error, "session alias realpath failed");
            }
            path.to_path_buf()
        }
    };
    PathBuf::from(resolved.to_string_lossy().nfc().collect::<String>())
}

/// Native OPr applies NFC inside the successful realpath branch. A failed
/// lookup retains the original path bytes, including decomposed characters.
fn alias_target_directory(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(resolved) => PathBuf::from(resolved.to_string_lossy().nfc().collect::<String>()),
        Err(error) => {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::debug!(path = %path.display(), %error, "session alias realpath failed");
            }
            path.to_path_buf()
        }
    }
}

/// Link the current session's authoritative transcript directory from an added
/// working directory. Returns whether a new line was appended.
///
/// `source_project_dir` is the actual transcript's parent, rather than a bucket
/// guessed from the current tool cwd: worktree entry and session relocation can
/// make those paths differ. Callers with transcript persistence disabled do not
/// invoke this operation. The source path is persisted verbatim, followed by LF.
/// A successful append has completed before this operation returns.
pub async fn record_session_alias(
    lingxi_home: &Path,
    source_project_dir: &Path,
    added_dir: &Path,
) -> io::Result<bool> {
    let added_dir = alias_target_directory(added_dir);
    let project_dir = lingxi_home
        .join("projects")
        .join(project_dir_name(&added_dir.to_string_lossy()));
    if source_project_dir.as_os_str() == project_dir.as_os_str() {
        return Ok(false);
    }
    // The write path only treats ENOENT as an absent file. Other read errors
    // stop the update; the read-only W0e-style API above remains best-effort.
    let content = match tokio::fs::read_to_string(project_dir.join(SESSION_ALIASES_FILE)).await {
        Ok(content) => content,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    let source = source_project_dir.to_string_lossy();
    if content.split('\n').any(|line| line == source.as_ref()) {
        return Ok(false);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&project_dir)?;
    }
    #[cfg(not(unix))]
    tokio::fs::create_dir_all(&project_dir).await?;

    let mut options = tokio::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(project_dir.join(SESSION_ALIASES_FILE)).await?;
    let line = format!("{source}\n");
    file.write_all(line.as_bytes()).await?;
    // Tokio write_all may only enqueue the blocking write. Native awaits
    // appendFile, so wait for completion before readers or a duplicate check.
    file.flush().await?;
    Ok(true)
}
